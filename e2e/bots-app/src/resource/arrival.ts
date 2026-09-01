export interface ArrivalSpread {
  count: number;
  firstJoinMs: number;
  lastJoinMs: number;
  spreadMs: number;
}

/** Records each bot's FIRST join; a later rejoin must not widen the spread. */
export class ArrivalTracker {
  private readonly joins = new Map<string, number>();

  constructor(private readonly now: () => number = () => Date.now()) {}

  record(botId: string, at: number = this.now()): void {
    if (!this.joins.has(botId)) this.joins.set(botId, at);
  }

  /** Distinct bots seen to reach in-meeting. */
  get joinedBots(): number {
    return this.joins.size;
  }

  snapshot(): ArrivalSpread | null {
    return arrivalSpread(Array.from(this.joins.values()));
  }
}

export function arrivalSpread(joins: readonly number[]): ArrivalSpread | null {
  const finite = joins.filter((t) => Number.isFinite(t));
  if (finite.length === 0) return null;
  const firstJoinMs = Math.min(...finite);
  const lastJoinMs = Math.max(...finite);
  return { count: finite.length, firstJoinMs, lastJoinMs, spreadMs: lastJoinMs - firstJoinMs };
}
