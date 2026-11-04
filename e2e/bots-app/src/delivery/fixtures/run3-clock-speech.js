(() => {
  "use strict";

  const TURN_SECONDS = 10;
  const PHRASE_GAP_SECONDS = 0.08;
  const SECONDS_PER_DAY = 86_400;
  const FILLERS = ["f-um", "f-uh"];
  const FILLER_PER_1024 = 341;
  const BACKCHANNELS = [
    "b-mmhm",
    "b-uhhuh",
    "b-yeah",
    "b-right",
    "b-okay",
    "b-sure",
    "b-gotit",
    "b-exactly",
  ];
  // One candidate per slot, starting in the slot's first 2 s, so starts are always >= 2 s apart.
  const BACKCHANNEL_SLOT_SECONDS = 4;
  const BACKCHANNEL_START_STEPS = 8;
  const BACKCHANNEL_STEP_SECONDS = 0.25;
  const BACKCHANNEL_PER_1024 = 820;
  const BACKCHANNEL_QUIET_SECONDS = 3;
  const BACKCHANNEL_LATEST_SECONDS = 9;
  // Weighted replay restarts at the top of each UTC hour, so a late joiner agrees.
  const ANCHOR_BLOCKS = 3_600 / TURN_SECONDS;
  // A speaker this many blocks over its share hands off even when the keep draw says stay.
  const OVER_SHARE_BLOCKS = 1;
  const MAX_LEAD_MS = 1_500;
  const SALT = { order: 0x0bd3e1, first: 0xf1257, keep: 0x6ee9, pick: 0x91c6, overlap: 0x0e1a9 };
  const PLAN = Object.freeze(globalThis.__CLOCK_SPEECH_PLAN ?? { model: "rotation" });
  // Fleet indices that are muted while listening (issue 3091); their backchannels are dropped.
  const MUTED_LISTENERS = new Set(globalThis.__CLOCK_MUTED_LISTENERS ?? []);
  if (PLAN.model !== "rotation" && PLAN.model !== "weighted") {
    throw new Error(`clock-speech: unknown speech model ${String(PLAN.model)}`);
  }

  const pad = (n) => String(n).padStart(2, "0");
  const numberClip = (n) => `n${pad(n)}`;
  const minuteClip = (m) => (m < 10 ? `m${pad(m)}` : numberClip(m));

  /** Deterministic 32-bit mix of an integer, identical in every bot. */
  function mix(n, salt) {
    let x = Math.imul(n | 0, 0x9e3779b1) ^ salt;
    x = Math.imul(x ^ (x >>> 16), 0x7feb352d);
    x = Math.imul(x ^ (x >>> 15), 0x846ca68b);
    return (x ^ (x >>> 16)) >>> 0;
  }

  function utcParts(epochSeconds) {
    const ofDay = ((epochSeconds % SECONDS_PER_DAY) + SECONDS_PER_DAY) % SECONDS_PER_DAY;
    return {
      hour: Math.floor(ofDay / 3_600),
      minute: Math.floor((ofDay % 3_600) / 60),
      second: ofDay % 60,
    };
  }

  const unit = (r) => r / 4_294_967_296;
  const sum = (values) => values.reduce((a, b) => a + b, 0);

  /** Index drawn in proportion to the non-negative `weights`; at least one must be positive. */
  function pick(weights, r) {
    let u = unit(r) * sum(weights);
    let last = -1;
    for (let i = 0; i < weights.length; i += 1) {
      if (weights[i] <= 0) continue;
      last = i;
      if (u < weights[i]) return i;
      u -= weights[i];
    }
    return last;
  }

  /** Talking share per fleet index, summing to 1: seeded order, dominant first, then observers. */
  function weightsFor(fleetSize, plan) {
    if (plan.model === "rotation") return new Array(fleetSize).fill(1 / fleetSize);
    const order = Array.from({ length: fleetSize }, (_, i) => i);
    for (let i = fleetSize - 1; i > 0; i -= 1) {
      const j = mix(i, mix(plan.seed, SALT.order)) % (i + 1);
      [order[i], order[j]] = [order[j], order[i]];
    }
    const dominant = plan.dominant.slice(0, fleetSize);
    const observers = Math.min(plan.observers, fleetSize - dominant.length);
    const rest = fleetSize - dominant.length - observers;
    // A non-dominant talker never out-talks a dominant one, so small rooms renormalise.
    const restShare = rest === 0 ? 0 : Math.min((1 - sum(dominant)) / rest, ...dominant);
    const raw = new Array(fleetSize).fill(restShare);
    dominant.forEach((share, k) => (raw[order[k]] = share));
    for (let k = dominant.length; k < dominant.length + observers; k += 1) raw[order[k]] = 0;
    const total = sum(raw);
    return raw.map((share) => share / total);
  }

  /** Speaker of each block of the hour starting at `anchor`. */
  function replayHour(anchor, plan, weights) {
    const salt = (s) => mix(plan.seed, s);
    const spoken = new Array(weights.length).fill(0);
    const speakers = new Array(ANCHOR_BLOCKS);
    let speaker = pick(weights, mix(anchor, salt(SALT.first)));
    for (let k = 0; k < ANCHOR_BLOCKS; k += 1) {
      if (k > 0) {
        const owed = (i) => weights[i] * (k + 1) - spoken[i];
        const keep = unit(mix(anchor + k, salt(SALT.keep))) < plan.keep;
        if (!keep || owed(speaker) <= -OVER_SHARE_BLOCKS) {
          const others = weights.map((_, i) => (i === speaker ? 0 : Math.max(0, owed(i))));
          if (sum(others) > 0) speaker = pick(others, mix(anchor + k, salt(SALT.pick)));
        }
      }
      speakers[k] = speaker;
      spoken[speaker] += 1;
    }
    return speakers;
  }

  /** Lead (s) per block: how early that block's new speaker starts; overlaps track `plan.overlap` per hour. */
  function handoffLeads(anchor, plan, speakers, previousLast) {
    const leads = new Array(ANCHOR_BLOCKS).fill(0);
    let handoffs = 0;
    let overlaps = 0;
    for (let k = 0; k < ANCHOR_BLOCKS; k += 1) {
      if (speakers[k] === (k === 0 ? previousLast : speakers[k - 1])) continue;
      handoffs += 1;
      const r = mix(anchor + k, mix(plan.seed, SALT.overlap));
      if (unit(r) < plan.overlap * handoffs - overlaps) {
        overlaps += 1;
        leads[k] = (1 + (mix(r, SALT.overlap) % MAX_LEAD_MS)) / 1_000;
      }
    }
    return leads;
  }

  /** The schedule every bot computes for `plan` and `fleetSize`; `blockAt` is its only reader. */
  function createSchedule(plan, fleetSize) {
    const weights = Object.freeze(weightsFor(fleetSize, plan));
    const hours = new Map();
    const speakersOf = (anchor) => {
      if (!hours.has(anchor)) {
        if (hours.size > 4) hours.clear();
        hours.set(anchor, { speakers: replayHour(anchor, plan, weights), leads: null });
      }
      return hours.get(anchor);
    };
    function blockAt(block) {
      if (plan.model === "rotation") return { speaker: block % fleetSize, lead: 0 };
      const offset = ((block % ANCHOR_BLOCKS) + ANCHOR_BLOCKS) % ANCHOR_BLOCKS;
      const anchor = block - offset;
      const hour = speakersOf(anchor);
      if (hour.leads === null) {
        const previousLast = speakersOf(anchor - ANCHOR_BLOCKS).speakers[ANCHOR_BLOCKS - 1];
        hour.leads = handoffLeads(anchor, plan, hour.speakers, previousLast);
      }
      return { speaker: hour.speakers[offset], lead: hour.leads[offset] };
    }
    return Object.freeze({ weights, blockAt });
  }

  const schedules = new Map();
  /** This page's speaker and lead for the turn containing `epochSeconds`. */
  function turnPlanFor(epochSeconds, fleetSize) {
    if (!schedules.has(fleetSize)) schedules.set(fleetSize, createSchedule(PLAN, fleetSize));
    return schedules.get(fleetSize).blockAt(Math.floor(epochSeconds / TURN_SECONDS));
  }

  function speakerForSecond(epochSeconds, fleetSize) {
    return turnPlanFor(epochSeconds, fleetSize).speaker;
  }

  function turnStartFor(epochSeconds) {
    return epochSeconds - (((epochSeconds % TURN_SECONDS) + TURN_SECONDS) % TURN_SECONDS);
  }

  function fillerForTurn(turnStart) {
    const r = mix(Math.floor(turnStart / TURN_SECONDS), 0x51f15e);
    return (r & 0x3ff) < FILLER_PER_1024 ? FILLERS[(r >>> 10) & 1] : null;
  }

  function backchannelSlotFor(epochSeconds) {
    return Math.floor(epochSeconds / BACKCHANNEL_SLOT_SECONDS);
  }

  /** The room's one possible backchannel in `slot`: `{ start, listener, clip }`, or null. */
  function backchannelForSlot(slot, fleetSize) {
    if (fleetSize < 2) return null;
    const r = mix(slot, 0xbac4c4a);
    if ((r & 0x3ff) >= BACKCHANNEL_PER_1024) return null;
    const step = (r >>> 10) % BACKCHANNEL_START_STEPS;
    const start = slot * BACKCHANNEL_SLOT_SECONDS + step * BACKCHANNEL_STEP_SECONDS;
    const intoTurn = start - turnStartFor(Math.floor(start));
    if (intoTurn < BACKCHANNEL_QUIET_SECONDS || intoTurn >= BACKCHANNEL_LATEST_SECONDS) {
      return null;
    }
    const speaker = speakerForSecond(Math.floor(start), fleetSize);
    const next = turnPlanFor(Math.floor(start) + TURN_SECONDS, fleetSize);
    const opensEarly = next.lead > 0 ? next.speaker : speaker;
    const listeners = [];
    for (let k = 1; k < fleetSize; k += 1) {
      const candidate = (speaker + k) % fleetSize;
      if (candidate !== opensEarly) listeners.push(candidate);
    }
    if (listeners.length === 0) return null;
    const listener = listeners[(r >>> 16) % listeners.length];
    if (MUTED_LISTENERS.has(listener)) return null;
    return { start, listener, clip: BACKCHANNELS[(r >>> 13) % BACKCHANNELS.length] };
  }

  /** Clips for one turn; `at` is seconds after `turnStart`, the phrase opening `lead` s early. A count second that would overlap the full-time phrase is dropped. */
  function turnClips(turnStart, durationOf, lead = 0) {
    const { hour, minute, second } = utcParts(turnStart);
    const clips = [];
    let at = lead > 0 ? -lead : 0;
    const filler = fillerForTurn(turnStart);
    const phrase = [numberClip(hour), minuteClip(minute), numberClip(second)];
    for (const clip of filler === null ? phrase : [filler, ...phrase]) {
      if (clips.length > 0) at += PHRASE_GAP_SECONDS;
      clips.push({ clip, at });
      at += durationOf(clip) ?? 0;
    }
    for (let offset = 1; offset < TURN_SECONDS; offset += 1) {
      if (offset < at) continue;
      clips.push({ clip: numberClip(utcParts(turnStart + offset).second), at: offset });
    }
    return clips;
  }

  globalThis.__CLOCK_SPEECH = Object.freeze({
    TURN_SECONDS,
    model: PLAN.model,
    mix,
    createSchedule,
    turnPlanFor,
    speakerForSecond,
    turnStartFor,
    turnClips,
    fillerForTurn,
    backchannelSlotFor,
    backchannelForSlot,
  });
})();
