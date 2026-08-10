# Contextual draft analysis

ArenaNext’s differentiator is not an LLM choosing among three names. It is a
local, deterministic draft-analysis engine that can optionally ask an AI model
to explain verified trade-offs.

## Three independent layers

1. Evidence: provider scores, win rates, sample sizes, timestamps, season and
   disagreement. These are facts with source metadata, not AI output.
2. Deck analysis: a deterministic local feature vector calculated from the
   current deck, Arena rules, card metadata and an offered-card counterfactual.
3. Explanation: an optional concise AI response over the structured facts. It
   cannot update card IDs, scores, deck counts, or the recommendation state.

The product must remain useful with no provider data, no network, or no AI
configuration. In those states it shows exactly what is absent rather than
inventing a score or card interaction.

## Local analysis contract

Before any model call, the analysis engine should calculate and serialize:

```json
{
  "pick_number": 14,
  "hero": "mage",
  "deck_profile": {
    "mana_curve": { "0-2": 2, "3": 3, "4-6": 7, "7+": 2 },
    "early_game_density": 2,
    "removal": 5,
    "card_draw": 1,
    "expected_speed": "midrange",
    "weaknesses": ["insufficient early board presence"]
  },
  "offers": [
    {
      "card_id": "EXAMPLE_001",
      "provider_evidence": [],
      "deck_delta": { "three_drops": 1 },
      "verified_synergies": [],
      "verified_anti_synergies": []
    }
  ]
}
```

The feature engine—not a model—owns arithmetic, curve counts, card text,
mechanic tags, Arena eligibility, and provider freshness. It must include an
explicit uncertainty state whenever metadata or rules are missing.

## Implemented local analysis boundary

`crates/arena-analysis` is the pure first implementation of this contract. It
accepts observed card IDs/counts, an optional expected deck size, the local
card cache, and a separate local semantic-facts file. It produces a
JSON-serializable curve, early-game density, removal/draw/reach/taunt/
stabilization counts, named weaknesses, and explicit uncertainty for missing
cards, missing facts, or unobserved slots.

It only counts facts supplied in the separate versioned facts file. This keeps
semantic facts independent from card metadata: refreshing card names or costs
cannot silently change a verified mechanic classification. It does not scrape
card names or hallucinate mechanics from card text. The initial vocabulary is:

```text
removal, board_clear, card_draw, discover, generation, reach, taunt,
stabilization, token, deathrattle
```

Season data may add `synergy:<category>` and `requires:<category>` (for
example `synergy:elemental`, `requires:elemental`). Counterfactual pick output
then reports the category count before/after the pick, its explicit
commitment, and future categories it makes more relevant. Unknown metadata
returns uncertainty, never invented synergies or a fabricated score.

## Draft optionality

The engine should quantify how each pick changes future category values and
how strongly it commits the deck. Examples include thresholds for dragons,
deathrattles, token payoffs, early curve, removal density, and late-game
value. A generic standalone card normally preserves more optionality; a
narrow synergy card can increase upside while reducing it. These are computed
signals that the explanation layer may describe, not conclusions fabricated by
the model.

## Optional AI boundary

An AI provider is opt-in and receives a versioned structured request only.
It must return validated structured output containing a recommendation,
confidence, concise per-option reasoning, provider caveats, and uncertainty.
It must not be asked to recall card text, calculate totals, or invent sources.

No provider, endpoint, key storage scheme, user-data retention policy, or
network call is selected yet. Adding one requires an explicit product/privacy
decision. Static card commentary may be cached locally; contextual analysis is
keyed by the deterministic input hash.

## Personalization

Playstyle preferences are a separate local profile. The UI can show both the
statistical recommendation and a preference-adjusted recommendation, along
with the reason for any difference. Learning from pick/run outcomes is opt-in;
raw logs or personal game history are never uploaded automatically.
