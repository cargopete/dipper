/* Choosing a release, so nobody has to read a list of them.
 *
 * The question "which of these will actually play" has an arithmetic answer:
 * a release streams if the swarm can supply its bitrate, and its bitrate is its
 * size divided by the episode's runtime. TVmaze gives the runtime, apibay gives
 * the size, so this is not a guess.
 *
 * What it cannot know is the connection, which is why the target is a parameter
 * with an honest default rather than something clever. Everything here is a
 * stated rule; nothing is a score with magic weights in it. */

import type { Hit } from "./apibay";

/** What a thin line sustains, in bytes per second. 1.5 Mbit/s. */
export const THIN_LINE_BYTES = 1_500_000 / 8;

export type Choice = {
  hit: Hit;
  /** Bytes per second this release needs to stream. */
  bitrate: number;
  /** True when nothing fitted the target and this is the least bad. */
  overBudget: boolean;
  /** How many releases were considered, for saying so out loud. */
  considered: number;
  why: string;
};

/** A release nobody wants, whatever its seeder count. */
function isCam(hit: Hit): boolean {
  if (hit.categoryLabel === "Cam / telesync") return true;
  // The category is often wrong, so the name gets a look too. Word boundaries,
  // or "camera" and "hdcam" catch things they should not.
  return /\b(cam|ts|telesync|hdts|camrip)\b/i.test(hit.name);
}

/**
 * Pick one, or null when there is nothing worth picking.
 *
 * `runtimeMinutes` comes from the catalogue. Without it there is no bitrate and
 * therefore no way to tell a tight 500MB rip from a 12GB remux except by size
 * alone, which is why it is required rather than optional.
 */
export function pick(
  hits: Hit[],
  runtimeMinutes: number,
  targetBytesPerSecond = THIN_LINE_BYTES,
): Choice | null {
  const seconds = Math.max(runtimeMinutes, 1) * 60;
  const candidates = hits
    .filter((hit) => hit.seeders > 0 && hit.sizeBytes > 0)
    .filter((hit) => !isCam(hit))
    .map((hit) => ({ hit, bitrate: hit.sizeBytes / seconds }));

  if (candidates.length === 0) return null;

  const fitting = candidates.filter((c) => c.bitrate <= targetBytesPerSecond);

  if (fitting.length > 0) {
    /* Among those that will play, the most seeded, because seeders decide
       whether it starts at all. Ties go to the larger file: within a budget
       already met, more bytes is more picture. */
    const best = fitting.reduce((a, b) =>
      b.hit.seeders !== a.hit.seeders
        ? b.hit.seeders > a.hit.seeders
          ? b
          : a
        : b.hit.sizeBytes > a.hit.sizeBytes
          ? b
          : a,
    );
    return {
      ...best,
      overBudget: false,
      considered: candidates.length,
      why: `best seeded of ${fitting.length} that fit`,
    };
  }

  /* Nothing fits. Take the smallest, because it is the one with any chance,
     and say plainly that it is over budget rather than starting it and letting
     the stalling explain itself. */
  const smallest = candidates.reduce((a, b) =>
    b.bitrate !== a.bitrate
      ? b.bitrate < a.bitrate
        ? b
        : a
      : b.hit.seeders > a.hit.seeders
        ? b
        : a,
  );
  return {
    ...smallest,
    overBudget: true,
    considered: candidates.length,
    why: "nothing fits a thin line; this is the smallest",
  };
}

/** Bytes per second as something a person reads. */
export function rateLabel(bytesPerSecond: number): string {
  const mbit = (bytesPerSecond * 8) / 1_000_000;
  return mbit >= 10 ? `${Math.round(mbit)} Mbit/s` : `${mbit.toFixed(1)} Mbit/s`;
}
