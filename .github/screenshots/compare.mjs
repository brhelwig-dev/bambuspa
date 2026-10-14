// Compares the shots taken from the pull request against the same shots taken
// from its base branch, and keeps only the ones that look different.
//
// Both sets come from the same harness, so a difference is the page changing,
// not the states being staged differently. A shot the base has no counterpart
// for (a new screen, or a base that could not be captured at all) counts as
// changed.
import { copyFile, mkdir, readFile, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { PNG } from "pngjs";
import pixelmatch from "pixelmatch";

const HEAD = process.env.HEAD_DIR || "shots/head";
const BASE = process.env.BASE_DIR || "shots/base";
const OUT = process.env.OUT || "shots/changed";

const shots = JSON.parse(await readFile(`${HEAD}/shots.json`, "utf8"));
const haveBase = existsSync(`${BASE}/shots.json`);

// How many pixels differ, or null when the two cannot be laid over each other.
async function differs(a, b) {
  const [x, y] = await Promise.all([a, b].map(async f => PNG.sync.read(await readFile(f))));
  if (x.width !== y.width || x.height !== y.height) return null;
  return pixelmatch(x.data, y.data, null, x.width, x.height);
}

await mkdir(OUT, { recursive: true });
const changed = [];
for (const shot of shots) {
  const head = `${HEAD}/${shot.name}.png`;
  const base = `${BASE}/${shot.name}.png`;
  let reason;
  if (!haveBase) {
    reason = "no baseline";
  } else if (!existsSync(base)) {
    reason = "new";
  } else {
    const n = await differs(head, base);
    if (n === null) reason = "resized";
    else if (n > 0) reason = "changed";
  }
  if (!reason) {
    console.log(`unchanged: ${shot.title}`);
    continue;
  }
  console.log(`${reason}: ${shot.title}`);
  await copyFile(head, `${OUT}/${shot.name}.png`);
  if (existsSync(base)) await copyFile(base, `${OUT}/${shot.name}.base.png`);
  changed.push({ ...shot, reason });
}

await writeFile(`${OUT}/changed.json`, JSON.stringify({
  total: shots.length,
  baseline: haveBase,
  changed,
}, null, 2) + "\n");

console.log(`${changed.length} of ${shots.length} shot(s) changed`);
