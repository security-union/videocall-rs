/**
 * The raise-hand chime's pitches (issue 2329) — ONE definition, imported by
 * every spec that has to reason about them.
 *
 * WHY THIS MODULE EXISTS, rather than a constant in each spec. Two specs need
 * these numbers for opposite reasons:
 *
 *   - `raise-hand.spec.ts` asserts the chime SOUNDS, and asserts which one.
 *   - `meeting-timer.spec.ts` asserts the chime is ABSENT, so that its own
 *     `=== 880` expiry fingerprint stays attributable to the timer.
 *
 * While the pitches were duplicated across the two, the second copy was the
 * dangerous one, because its failure is SILENT: a frequency filter left on a
 * retired pitch matches nothing, counts zero, and therefore PASSES — reporting
 * "no hand chime interfered" for the one reason that would make the report
 * worthless. Exactly that trap was live when the chime moved from A5/D6 to
 * B5/E6.
 *
 * This module removes the second copy outright, and that is the point of the
 * extraction. Two distinct guarantees follow, and they are worth separating:
 *
 *   - A pitch CHANGE is one edit that both specs read. Nothing is left to go
 *     stale, because there is no longer a second place to forget.
 *   - The import is a hard dependency, not a convention. Delete or rename an
 *     export and BOTH specs fail to compile — verified by mutation: renaming
 *     `HAND_TONE_HIGH` yields one `tsc` error in each — rather than one of them
 *     silently filtering on a frequency nothing emits any more.
 *
 * KEEP THIS FILE IN STEP WITH `play_hand_raised` / `play_hand_lowered` in
 * `dioxus-ui/src/components/attendants.rs`. Those functions are the source of
 * truth; everything below is a mirror of them, and a mirror is only worth
 * having while somebody re-reads the original.
 */

/**
 * B5 — the low endpoint. `play_hand_raised` opens on it; `play_hand_lowered`
 * closes on it.
 */
export const HAND_TONE_LOW = 987.77;

/**
 * E6 — the high endpoint. A perfect fourth above B5, and the interval is what
 * separates the hand chimes from the join chime's major third and the leave
 * chime's perfect fifth.
 */
export const HAND_TONE_HIGH = 1318.51;

/** Note names, used to render a recorded frequency run as a readable shape. */
export const HAND_TONE_LOW_LABEL = "B5";
export const HAND_TONE_HIGH_LABEL = "E6";

/** One recorded hand-chime pitch, as a note name. */
export type HandTone = typeof HAND_TONE_LOW_LABEL | typeof HAND_TONE_HIGH_LABEL;

/**
 * Frequencies are matched with a TOLERANCE, never with `===`. This is
 * load-bearing, not defensive, and it does not depend on which pitches are
 * chosen.
 *
 * `web_sys::AudioParam::set_value_at_time` takes an `f32`, so every `f64`
 * literal in `attendants.rs` is narrowed on its way into the browser and
 * widened again on the way back out. Neither hand pitch survives that trip
 * intact:
 *
 *     987.77  -> 987.77001953125
 *     1318.51 -> 1318.510009765625
 *
 * so a `value === 987.77` filter matches NOTHING. Every recorded sequence would
 * read empty, and every exact-zero assertion built on it would pass for the one
 * reason it must not — a spy that looks like it is working and is not.
 *
 * WHY 0.01 IS SAFE AT ANY PITCH, and therefore why this constant should not be
 * re-tuned when the notes change: the binary32 error across the musical range
 * peaks around 3.4e-5 and stays under 2e-3 even at 4186 Hz (C8), so the margin
 * here is roughly three orders of magnitude. It is nowhere near tight enough to
 * confuse two distinct notes either — the nearest semitone to B5 is ~55 Hz away.
 *
 * (Some frequencies DO survive exactly — 880, 440, 523.25, 659.25, 1046.5, all
 * being integers or quarter fractions — which is why `meeting-timer.spec.ts`
 * can use a bare `=== 880` for the expiry cue. That is a property of those
 * particular numbers, not something to rely on when picking new ones.)
 */
export const TONE_EPSILON = 0.01;

/**
 * A hand going UP: ascending, B5 -> E6.
 *
 * Composed from the label constants rather than written out as literals, so
 * renaming a note cannot leave a stale string behind in an assertion.
 */
export const RAISED_PAIR: HandTone[] = [HAND_TONE_LOW_LABEL, HAND_TONE_HIGH_LABEL];

/**
 * A hand coming DOWN: the exact retrograde, E6 -> B5.
 *
 * Being the reverse of [`RAISED_PAIR`] and not merely a different pair is the
 * whole reason the specs assert ORDERED sequences. The two chimes share both
 * pitches, so any assertion that only counted them would pass with the two
 * functions swapped — i.e. it would pass while users heard "hand lowered" every
 * time a hand went up.
 */
export const LOWERED_PAIR: HandTone[] = [HAND_TONE_HIGH_LABEL, HAND_TONE_LOW_LABEL];
