# HearthAI roadmap

HearthAI is the product name; Arena is the first supported workflow, not the
long-term boundary.

## Companion modes

1. Arena draft and Redraft: current vertical slice.
2. Constructed deck tracking: imported or log-reconstructed deck, mulligan and
   in-game hand/deck state, mana curve, fatigue and remaining-card estimates.
3. Turn companion: deterministic board snapshot, legal-card/resource summary,
   lethal and defensive checks, and explainable candidate lines.
4. Optional AI coach: interpret verified facts and compare lines; never invent
   card text, silently take actions, inject into Hearthstone, or click for the
   player.

The shared parser/reducer must remain mode-neutral. Arena rules, constructed
deck rules, ratings, and turn analysis belong in separate providers over the
same event/state protocol. The first constructed milestone should be a
read-only tracker and board summary before adding recommendations.
