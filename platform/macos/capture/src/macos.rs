//! The macOS implementation is intentionally ScreenCaptureKit-only for image
//! acquisition. CoreGraphics is used solely for its documented, non-prompting
//! Screen Recording permission probe/request and for CGImage byte extraction.
//!
//! There is no `CGWindowListCreateImage` fallback: that API is deprecated and
//! would make it too easy to regress into a desktop/window-server screenshot
//! path. Every image below originates from an
//! `initWithDesktopIndependentWindow:` ScreenCaptureKit filter.

use std::{
    sync::mpsc::{Receiver, SyncSender, sync_channel},
    time::Duration,
};

use block2::RcBlock;
use objc2::{AnyThread, MainThreadMarker, rc::autoreleasepool, runtime::AnyObject};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGDataProvider, CGImage, CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess,
};
use objc2_foundation::{NSArray, NSDictionary, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration, SCWindow,
};
use objc2_vision::{
    VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
};

use crate::{
    CaptureError, CaptureOptions, CaptureProvider, CapturedFrame, DeckSidebarCapture,
    DraftOfferTextCapture, FeatureAvailability, GameWindow, GameWindowProvider, PixelFormat,
    PlatformCapabilities, RecognizedText, ScreenRecordingPermission, WindowFrame, WindowId,
    is_hearthstone_bundle_id,
};

/// The minimum OS family supported by this ScreenCaptureKit-based leaf.
///
/// ArenaNext deliberately requires a current macOS with ScreenCaptureKit
/// rather than carrying a deprecated CoreGraphics image-capture fallback.
pub const MINIMUM_SUPPORTED_MACOS: &str = "macOS 13 or later";

// `NSApplicationLoad` is a documented AppKit initializer. The native overlay
// application normally calls `NSApplication::sharedApplication` first, which
// makes this redundant. The explicit helper below lets a small CLI probe
// initialize the WindowServer/AppKit runtime before it asks ScreenCaptureKit
// for an image.
#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    fn NSApplicationLoad() -> bool;
}

/// A zero-state, direct-window ScreenCaptureKit provider.
///
/// It stores no capture history and opens no prompt during construction or
/// capability checks. One-shot calls block until the system invokes a
/// completion handler, so use it on the draft-recognition worker rather than
/// the AppKit main thread.
#[derive(Clone, Copy, Debug, Default)]
pub struct MacosWindowCapture;

impl MacosWindowCapture {
    pub const fn new() -> Self {
        Self
    }

    /// Initializes the AppKit runtime required by ScreenCaptureKit image
    /// capture when this provider is hosted by a command-line tool.
    ///
    /// Call this once on the macOS main thread before dispatching capture work.
    /// The normal ArenaNext overlay already creates `NSApplication`, so calling
    /// this there is harmless but unnecessary. Window discovery alone does not
    /// require it; a bare CLI needs it before [`Self::capture_window`].
    pub fn initialize_appkit_runtime(&self) -> Result<(), CaptureError> {
        if MainThreadMarker::new().is_none() {
            return Err(CaptureError::AppKitInitialization(
                "must be called on the macOS main thread before capture workers start",
            ));
        }
        if unsafe { NSApplicationLoad() } {
            Ok(())
        } else {
            Err(CaptureError::AppKitInitialization(
                "NSApplicationLoad returned false",
            ))
        }
    }

    /// Returns the current permission-gated feature state without causing a
    /// macOS consent prompt.
    pub fn capabilities(&self) -> PlatformCapabilities {
        let availability = if self.screen_recording_permission().is_granted() {
            FeatureAvailability::Available
        } else {
            FeatureAvailability::PermissionRequired
        };

        PlatformCapabilities {
            hearthstone_window_discovery: availability,
            direct_window_capture: availability,
            full_desktop_capture: FeatureAvailability::DisabledByDesign(
                "ArenaNext only captures a ScreenCaptureKit-filtered Hearthstone window",
            ),
            process_memory_inspection: FeatureAvailability::DisabledByDesign(
                "ArenaNext never inspects Hearthstone process memory",
            ),
            input_injection: FeatureAvailability::DisabledByDesign(
                "ArenaNext never injects input into Hearthstone",
            ),
            requires_screen_recording_permission: true,
        }
    }

    /// Checks access without showing a system prompt.
    pub fn screen_recording_permission(&self) -> ScreenRecordingPermission {
        if CGPreflightScreenCaptureAccess() {
            ScreenRecordingPermission::Granted
        } else {
            ScreenRecordingPermission::Required
        }
    }

    /// Requests macOS Screen Recording access.
    ///
    /// This is the only API in this crate that may show a system prompt. Call
    /// it exclusively from an intentional user action in the settings UI;
    /// normal startup, discovery, and capture attempts only report
    /// [`ScreenRecordingPermission::Required`].
    pub fn request_screen_recording_access(&self) -> ScreenRecordingPermission {
        if CGRequestScreenCaptureAccess() {
            ScreenRecordingPermission::Granted
        } else {
            ScreenRecordingPermission::Required
        }
    }

    /// Finds shareable windows owned by retail Hearthstone.
    ///
    /// ScreenCaptureKit delivers the shareable-content list asynchronously.
    /// This helper waits only for the metadata result and returns plain Rust
    /// structs; no Objective-C window object crosses a thread boundary.
    pub fn find_hearthstone_windows(&self) -> Result<Vec<GameWindow>, CaptureError> {
        ensure_permission(self)?;
        let (sender, receiver) = sync_channel(1);
        let completion = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                autoreleasepool(|_| {
                    let result = shareable_hearthstone_windows(content, error);
                    let _ = sender.send(result);
                });
            },
        );

        // Ask for shareable windows across Spaces. Restricting this to the
        // current on-screen workspace hides a native-fullscreen Hearthstone
        // window as soon as the control app is in a different Space. We still
        // return metadata only for the recognized Hearthstone bundle IDs and
        // never construct a display-capture filter.
        unsafe {
            SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                true,
                false,
                &completion,
            );
        }

        receive(receiver, CaptureOptions::default().timeout)
    }

    /// Captures a fresh image for one Hearthstone window using a direct-window
    /// ScreenCaptureKit filter.
    ///
    /// The provided ID is re-checked against a current ScreenCaptureKit
    /// listing before a filter is constructed. Passing an ID for another app,
    /// a stale window, or a manually constructed [`GameWindow`] never enables
    /// an arbitrary-window capture.
    pub fn capture_window(
        &self,
        window: &GameWindow,
        options: CaptureOptions,
    ) -> Result<CapturedFrame, CaptureError> {
        options.validate()?;
        ensure_permission(self)?;
        if !window.is_hearthstone() {
            return Err(CaptureError::WindowNotFound(window.id));
        }

        let (sender, receiver) = sync_channel(1);
        let requested_window = window.id;
        let completion_sender = sender.clone();
        let completion = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                autoreleasepool(|_| {
                    if !error.is_null() {
                        let _ = completion_sender.send(Err(screen_capture_error(error)));
                        return;
                    }

                    if let Err(error) = begin_direct_window_capture(
                        content,
                        requested_window,
                        options,
                        CaptureMode::FrameOnly,
                        completion_sender.clone(),
                    ) {
                        let _ = completion_sender.send(Err(error));
                    }
                });
            },
        );

        unsafe {
            SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                true,
                false,
                &completion,
            );
        }

        match receive(receiver, options.timeout)? {
            CaptureResult::Frame(frame) => Ok(frame),
            CaptureResult::DeckSidebar(_) | CaptureResult::DraftOffers(_) => {
                unreachable!("frame-only capture returned OCR")
            }
        }
    }

    /// Captures the window and performs local OCR only on the right deck panel.
    pub fn capture_deck_sidebar(
        &self,
        window: &GameWindow,
        options: CaptureOptions,
    ) -> Result<DeckSidebarCapture, CaptureError> {
        options.validate()?;
        ensure_permission(self)?;
        if !window.is_hearthstone() {
            return Err(CaptureError::WindowNotFound(window.id));
        }

        let (sender, receiver) = sync_channel(1);
        let requested_window = window.id;
        let completion_sender = sender.clone();
        let completion = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                autoreleasepool(|_| {
                    if !error.is_null() {
                        let _ = completion_sender.send(Err(screen_capture_error(error)));
                        return;
                    }
                    if let Err(error) = begin_direct_window_capture(
                        content,
                        requested_window,
                        options,
                        CaptureMode::DeckSidebar,
                        completion_sender.clone(),
                    ) {
                        let _ = completion_sender.send(Err(error));
                    }
                });
            },
        );

        unsafe {
            SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                true,
                false,
                &completion,
            );
        }

        match receive(receiver, options.timeout)? {
            CaptureResult::DeckSidebar(capture) => Ok(capture),
            CaptureResult::Frame(_) | CaptureResult::DraftOffers(_) => {
                unreachable!("sidebar capture returned another mode")
            }
        }
    }

    /// Captures and locally OCRs the title bands of the three current offers.
    pub fn capture_draft_offer_text(
        &self,
        window: &GameWindow,
        options: CaptureOptions,
    ) -> Result<DraftOfferTextCapture, CaptureError> {
        options.validate()?;
        ensure_permission(self)?;
        if !window.is_hearthstone() {
            return Err(CaptureError::WindowNotFound(window.id));
        }

        let (sender, receiver) = sync_channel(1);
        let requested_window = window.id;
        let completion_sender = sender.clone();
        let completion = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                autoreleasepool(|_| {
                    if !error.is_null() {
                        let _ = completion_sender.send(Err(screen_capture_error(error)));
                        return;
                    }
                    if let Err(error) = begin_direct_window_capture(
                        content,
                        requested_window,
                        options,
                        CaptureMode::DraftOffers,
                        completion_sender.clone(),
                    ) {
                        let _ = completion_sender.send(Err(error));
                    }
                });
            },
        );
        unsafe {
            SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                true,
                false,
                &completion,
            );
        }
        match receive(receiver, options.timeout)? {
            CaptureResult::DraftOffers(capture) => Ok(capture),
            CaptureResult::Frame(_) | CaptureResult::DeckSidebar(_) => {
                unreachable!("draft-offer capture returned another mode")
            }
        }
    }
}

impl GameWindowProvider for MacosWindowCapture {
    fn capabilities(&self) -> PlatformCapabilities {
        self.capabilities()
    }

    fn find_hearthstone_windows(&self) -> Result<Vec<GameWindow>, CaptureError> {
        self.find_hearthstone_windows()
    }
}

impl CaptureProvider for MacosWindowCapture {
    fn capture_window(
        &self,
        window: &GameWindow,
        options: CaptureOptions,
    ) -> Result<CapturedFrame, CaptureError> {
        self.capture_window(window, options)
    }
}

fn ensure_permission(provider: &MacosWindowCapture) -> Result<(), CaptureError> {
    if provider.screen_recording_permission().is_granted() {
        Ok(())
    } else {
        Err(CaptureError::PermissionRequired)
    }
}

fn receive<T>(
    receiver: Receiver<Result<T, CaptureError>>,
    timeout: Duration,
) -> Result<T, CaptureError> {
    receiver
        .recv_timeout(timeout)
        .map_err(|_| CaptureError::TimedOut(timeout))?
}

fn shareable_hearthstone_windows(
    content: *mut SCShareableContent,
    error: *mut NSError,
) -> Result<Vec<GameWindow>, CaptureError> {
    if !error.is_null() {
        return Err(screen_capture_error(error));
    }
    let content = unsafe { content.as_ref() }.ok_or(CaptureError::ScreenCaptureKit(
        "shareable-content response was empty".to_owned(),
    ))?;
    let windows = unsafe { content.windows() };
    let mut hearthstone_windows = Vec::new();

    for index in 0..windows.count() {
        let window = windows.objectAtIndex(index);
        if let Some(window) = map_hearthstone_window(&window) {
            hearthstone_windows.push(window);
        }
    }

    // ScreenCaptureKit commonly lists Hearthstone title/menu child windows
    // before the main game surface. Put the largest valid surface first so a
    // simple consumer uses the real client window by default, while retaining
    // the complete list for diagnostics and future explicit selection UI.
    hearthstone_windows.sort_by(|left, right| {
        right
            .area_points()
            .total_cmp(&left.area_points())
            .then_with(|| left.id.cmp(&right.id))
    });

    Ok(hearthstone_windows)
}

fn begin_direct_window_capture(
    content: *mut SCShareableContent,
    requested_window: WindowId,
    options: CaptureOptions,
    mode: CaptureMode,
    sender: SyncSender<Result<CaptureResult, CaptureError>>,
) -> Result<(), CaptureError> {
    let content = unsafe { content.as_ref() }.ok_or(CaptureError::ScreenCaptureKit(
        "shareable-content response was empty".to_owned(),
    ))?;
    let window = hearthstone_window_by_id(content, requested_window)
        .ok_or(CaptureError::WindowNotFound(requested_window))?;
    let window_frame = map_window_frame(&window);
    if !window_frame.is_valid() {
        return Err(CaptureError::InvalidFrame(
            "ScreenCaptureKit reported a non-finite or empty Hearthstone window frame",
        ));
    }

    // This is the crucial safety boundary: an independent window filter, not
    // a display filter and not a crop from a desktop frame.
    let filter = unsafe {
        SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window)
    };
    // Objective-C's `new` allocation is marked unsafe by the binding because
    // this class is not main-thread-bound in the type system. It is created
    // and consumed entirely by this ScreenCaptureKit request.
    let configuration = unsafe { SCStreamConfiguration::new() };
    let (width_px, height_px) = output_dimensions(window_frame, options);
    configure_bgra_capture(&configuration, width_px, height_px);

    // ScreenCaptureKit retains its completion block. Keep the native filter
    // and configuration alive through that callback as an extra ownership
    // guarantee even if an OS implementation defers reading either object.
    let keep_filter = filter.clone();
    let keep_configuration = configuration.clone();
    let image_sender = sender.clone();
    let image_completion = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
        autoreleasepool(|_| {
            let _keep_alive = (&keep_filter, &keep_configuration);
            let result = (|| {
                if !error.is_null() {
                    Err(screen_capture_error(error))
                } else {
                    let frame = captured_frame_from_image(requested_window, image)?;
                    match mode {
                        CaptureMode::FrameOnly => Ok(CaptureResult::Frame(frame)),
                        CaptureMode::DeckSidebar => {
                            let image = unsafe { image.as_ref() }.ok_or(
                                CaptureError::InvalidFrame("ScreenCaptureKit returned no image"),
                            )?;
                            let text = recognize_deck_sidebar(image)?;
                            Ok(CaptureResult::DeckSidebar(DeckSidebarCapture {
                                frame,
                                text,
                            }))
                        }
                        CaptureMode::DraftOffers => {
                            let image = unsafe { image.as_ref() }.ok_or(
                                CaptureError::InvalidFrame("ScreenCaptureKit returned no image"),
                            )?;
                            // Reject non-draft screens before paying for the
                            // larger three-title OCR pass.
                            let header = recognize_draft_header(image)?;
                            let offers = recognize_draft_offers(image)?;
                            Ok(CaptureResult::DraftOffers(DraftOfferTextCapture {
                                frame,
                                header,
                                offers,
                            }))
                        }
                    }
                }
            })();
            let _ = image_sender.send(result);
        });
    });

    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            &filter,
            &configuration,
            Some(&image_completion),
        );
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum CaptureMode {
    FrameOnly,
    DeckSidebar,
    DraftOffers,
}

enum CaptureResult {
    Frame(CapturedFrame),
    DeckSidebar(DeckSidebarCapture),
    DraftOffers(DraftOfferTextCapture),
}

fn recognize_deck_sidebar(image: &CGImage) -> Result<Vec<RecognizedText>, CaptureError> {
    let width = CGImage::width(Some(image)) as f64;
    let height = CGImage::height(Some(image)) as f64;
    let crop = CGImage::with_image_in_rect(
        Some(image),
        CGRect::new(
            CGPoint::new(width * 0.70, height * 0.015),
            CGSize::new(width * 0.27, height * 0.97),
        ),
    )
    .ok_or(CaptureError::InvalidFrame(
        "could not crop the Hearthstone deck sidebar",
    ))?;
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(true);
    request.setMinimumTextHeight(0.012);

    let options = NSDictionary::<objc2_vision::VNImageOption, AnyObject>::new();
    let handler = unsafe {
        VNImageRequestHandler::initWithCGImage_options(
            VNImageRequestHandler::alloc(),
            &crop,
            &options,
        )
    };
    let requests = NSArray::<VNRequest>::from_slice(&[&request]);
    handler
        .performRequests_error(&requests)
        .map_err(|error| CaptureError::TextRecognition(error.localizedDescription().to_string()))?;

    let Some(results) = request.results() else {
        return Ok(Vec::new());
    };
    let mut text = Vec::with_capacity(results.count());
    for index in 0..results.count() {
        let observation = results.objectAtIndex(index);
        let candidates = observation.topCandidates(1);
        if candidates.count() == 0 {
            continue;
        }
        let candidate = candidates.objectAtIndex(0);
        let bounds = unsafe { observation.boundingBox() };
        text.push(RecognizedText {
            text: candidate.string().to_string(),
            confidence: candidate.confidence(),
            x: bounds.origin.x,
            y: bounds.origin.y,
            width: bounds.size.width,
            height: bounds.size.height,
        });
    }
    text.sort_by(|left, right| {
        right
            .y
            .total_cmp(&left.y)
            .then_with(|| left.x.total_cmp(&right.x))
    });
    Ok(text)
}

fn recognize_draft_offers(image: &CGImage) -> Result<[Vec<RecognizedText>; 3], CaptureError> {
    // Current Arena's three title ribbons occupy the left ~65% because the
    // committed-deck sidebar owns the right side. Vision ROIs use a normalized
    // lower-left origin; these correspond to the visible name bands in the
    // current 16:10 draft layout.
    // A single Accurate request over the combined title strip avoids paying
    // Vision's model setup cost three times. Observation coordinates remain
    // image-relative, allowing deterministic routing back to the three slots.
    const STRIP_X: f64 = 0.155;
    const STRIP_WIDTH: f64 = 0.510;
    const SLOT_WIDTH: f64 = 0.165;
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(true);
    request.setMinimumTextHeight(0.010);
    unsafe {
        request.setRegionOfInterest(CGRect::new(
            CGPoint::new(STRIP_X, 0.535),
            CGSize::new(STRIP_WIDTH, 0.125),
        ));
    }
    let request_array = NSArray::<VNRequest>::from_slice(&[&request]);
    let options = NSDictionary::<objc2_vision::VNImageOption, AnyObject>::new();
    let handler = unsafe {
        VNImageRequestHandler::initWithCGImage_options(
            VNImageRequestHandler::alloc(),
            image,
            &options,
        )
    };
    handler
        .performRequests_error(&request_array)
        .map_err(|error| CaptureError::TextRecognition(error.localizedDescription().to_string()))?;
    let mut offers: [Vec<RecognizedText>; 3] = std::array::from_fn(|_| Vec::new());
    for item in recognized_text_results(&request) {
        let center = item.x + item.width / 2.0;
        // Vision reports observation bounds relative to the configured ROI,
        // not the original full image. Divide that local strip into thirds.
        let slot = draft_offer_slot(center, STRIP_WIDTH / SLOT_WIDTH);
        offers[slot].push(item);
    }
    Ok(offers)
}

fn recognize_draft_header(image: &CGImage) -> Result<Vec<RecognizedText>, CaptureError> {
    // The current client omits the Arena Redraft boundary from Arena.log.
    // The centered top ribbon is therefore the authoritative visual signal
    // that this is the Draft New Cards editor rather than gameplay.
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(true);
    request.setMinimumTextHeight(0.012);
    unsafe {
        request.setRegionOfInterest(CGRect::new(
            // Include both the border ribbon and the macOS window-content
            // offset seen on current clients. The caller still requires the
            // distinctive header text, so the wider ROI cannot activate
            // draft OCR during ordinary gameplay.
            CGPoint::new(0.10, 0.76),
            CGSize::new(0.80, 0.23),
        ));
    }
    let request_array = NSArray::<VNRequest>::from_slice(&[&request]);
    let options = NSDictionary::<objc2_vision::VNImageOption, AnyObject>::new();
    let handler = unsafe {
        VNImageRequestHandler::initWithCGImage_options(
            VNImageRequestHandler::alloc(),
            image,
            &options,
        )
    };
    handler
        .performRequests_error(&request_array)
        .map_err(|error| CaptureError::TextRecognition(error.localizedDescription().to_string()))?;
    Ok(recognized_text_results(&request))
}

fn draft_offer_slot(roi_relative_center_x: f64, slot_count: f64) -> usize {
    ((roi_relative_center_x * slot_count).floor() as isize).clamp(0, 2) as usize
}

fn recognized_text_results(request: &VNRecognizeTextRequest) -> Vec<RecognizedText> {
    let Some(results) = request.results() else {
        return Vec::new();
    };
    let mut text = Vec::with_capacity(results.count());
    for index in 0..results.count() {
        let observation = results.objectAtIndex(index);
        let candidates = observation.topCandidates(1);
        if candidates.count() == 0 {
            continue;
        }
        let candidate = candidates.objectAtIndex(0);
        let bounds = unsafe { observation.boundingBox() };
        text.push(RecognizedText {
            text: candidate.string().to_string(),
            confidence: candidate.confidence(),
            x: bounds.origin.x,
            y: bounds.origin.y,
            width: bounds.size.width,
            height: bounds.size.height,
        });
    }
    text.sort_by(|left, right| {
        right
            .y
            .total_cmp(&left.y)
            .then_with(|| left.x.total_cmp(&right.x))
    });
    text
}

fn map_hearthstone_window(window: &SCWindow) -> Option<GameWindow> {
    let owner = unsafe { window.owningApplication() }?;
    let owner_bundle_id = unsafe { owner.bundleIdentifier() }.to_string();
    if !is_hearthstone_bundle_id(&owner_bundle_id) {
        return None;
    }

    let title = unsafe { window.title() }.map(|title| title.to_string());
    Some(GameWindow {
        id: WindowId(unsafe { window.windowID() }),
        owner_bundle_id,
        owner_name: unsafe { owner.applicationName() }.to_string(),
        title,
        frame: map_window_frame(window),
        window_layer: unsafe { window.windowLayer() } as i64,
        on_screen: unsafe { window.isOnScreen() },
        active: unsafe { window.isActive() },
    })
}

fn hearthstone_window_by_id(
    content: &SCShareableContent,
    requested_window: WindowId,
) -> Option<objc2::rc::Retained<SCWindow>> {
    let windows = unsafe { content.windows() };
    for index in 0..windows.count() {
        let window = windows.objectAtIndex(index);
        if WindowId(unsafe { window.windowID() }) == requested_window
            && map_hearthstone_window(&window).is_some()
        {
            return Some(window);
        }
    }
    None
}

fn map_window_frame(window: &SCWindow) -> WindowFrame {
    let frame = unsafe { window.frame() };
    WindowFrame {
        x_points: frame.origin.x,
        y_points: frame.origin.y,
        width_points: frame.size.width,
        height_points: frame.size.height,
    }
}

fn configure_bgra_capture(
    configuration: &SCStreamConfiguration,
    width_px: usize,
    height_px: usize,
) {
    // `captureImageWithFilter` returns BGRA for an SDR stream. Set it
    // explicitly so card recognition receives a predictable four-byte format.
    const BGRA: u32 = u32::from_be_bytes(*b"BGRA");
    unsafe {
        configuration.setWidth(width_px);
        configuration.setHeight(height_px);
        configuration.setPixelFormat(BGRA);
        configuration.setShowsCursor(false);
        configuration.setCapturesAudio(false);
        configuration.setIncludeChildWindows(false);
        configuration.setIgnoreShadowsSingleWindow(true);
        configuration.setIgnoreGlobalClipSingleWindow(true);
    }
}

fn output_dimensions(frame: WindowFrame, options: CaptureOptions) -> (usize, usize) {
    // ScreenCaptureKit frames are in points. Start with a conservative 2x
    // backing-scale estimate so Retina card art remains useful, then bound the
    // temporary BGRA buffer to the caller's explicit maximum dimensions.
    let desired_width = (frame.width_points * 2.0).ceil().max(1.0);
    let desired_height = (frame.height_points * 2.0).ceil().max(1.0);
    let scale = (options.max_width_px as f64 / desired_width)
        .min(options.max_height_px as f64 / desired_height)
        .min(1.0);
    let width = (desired_width * scale).round().max(1.0) as usize;
    let height = (desired_height * scale).round().max(1.0) as usize;
    (width, height)
}

fn captured_frame_from_image(
    window_id: WindowId,
    image: *mut CGImage,
) -> Result<CapturedFrame, CaptureError> {
    let image = unsafe { image.as_ref() }.ok_or(CaptureError::InvalidFrame(
        "ScreenCaptureKit returned no image",
    ))?;
    let width_px = u32::try_from(CGImage::width(Some(image)))
        .map_err(|_| CaptureError::InvalidFrame("image width exceeds u32"))?;
    let height_px = u32::try_from(CGImage::height(Some(image)))
        .map_err(|_| CaptureError::InvalidFrame("image height exceeds u32"))?;
    let bytes_per_row = CGImage::bytes_per_row(Some(image));
    if width_px == 0 || height_px == 0 {
        return Err(CaptureError::InvalidFrame("image dimensions are empty"));
    }
    if CGImage::bits_per_pixel(Some(image)) != 32 {
        return Err(CaptureError::InvalidFrame(
            "ScreenCaptureKit did not return the configured 32-bit BGRA image",
        ));
    }

    let row_bytes = (width_px as usize)
        .checked_mul(4)
        .ok_or(CaptureError::InvalidFrame("row width overflow"))?;
    if bytes_per_row < row_bytes {
        return Err(CaptureError::InvalidFrame(
            "image bytes-per-row is smaller than a BGRA row",
        ));
    }
    let required_bytes = bytes_per_row
        .checked_mul(height_px as usize)
        .ok_or(CaptureError::InvalidFrame("image byte count overflow"))?;

    let provider = CGImage::data_provider(Some(image))
        .ok_or(CaptureError::InvalidFrame("image has no data provider"))?;
    let data = CGDataProvider::data(Some(&provider)).ok_or(CaptureError::InvalidFrame(
        "image data provider returned no bytes",
    ))?;
    let mut pixels = data.to_vec();
    if pixels.len() < required_bytes {
        return Err(CaptureError::InvalidFrame(
            "image data provider returned fewer bytes than the image geometry requires",
        ));
    }
    pixels.truncate(required_bytes);

    Ok(CapturedFrame {
        window_id,
        width_px,
        height_px,
        bytes_per_row,
        pixel_format: PixelFormat::Bgra8,
        pixels,
    })
}

fn screen_capture_error(error: *mut NSError) -> CaptureError {
    let description = unsafe { error.as_ref() }
        .map(|error| error.localizedDescription().to_string())
        .unwrap_or_else(|| "ScreenCaptureKit returned an unspecified error".to_owned());
    CaptureError::ScreenCaptureKit(description)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_dimensions_preserve_aspect_ratio_within_limit() {
        let frame = WindowFrame {
            x_points: 0.0,
            y_points: 0.0,
            width_points: 1_920.0,
            height_points: 1_080.0,
        };
        let (width, height) = output_dimensions(frame, CaptureOptions::default());
        assert_eq!((width, height), (2_560, 1_440));
    }

    #[test]
    fn output_dimensions_do_not_upscale_small_windows_past_two_x() {
        let frame = WindowFrame {
            x_points: 0.0,
            y_points: 0.0,
            width_points: 500.0,
            height_points: 300.0,
        };
        let (width, height) = output_dimensions(frame, CaptureOptions::default());
        assert_eq!((width, height), (1_000, 600));
    }

    #[test]
    fn vision_roi_relative_title_centers_route_to_three_distinct_slots() {
        // Measured from a current Arena frame: Vision bounds are relative to
        // the combined title-strip ROI, not the original full window.
        assert_eq!(draft_offer_slot(0.166, 3.0), 0);
        assert_eq!(draft_offer_slot(0.488, 3.0), 1);
        assert_eq!(draft_offer_slot(0.799, 3.0), 2);
    }
}
