// After `vite build`: the prerendered app shell (_shell.html) links the
// stylesheet by the hash the server build gave it, which differs from the
// file the client build wrote. Point it at the real file, so pages are
// styled from the first paint instead of only once the JavaScript runs.
import { readdirSync, readFileSync, writeFileSync } from "node:fs";

const dir = "dist/client";
const shellPath = `${dir}/_shell.html`;
const built = readdirSync(`${dir}/assets`).filter((f) => /^styles-.*\.css$/.test(f));
if (built.length !== 1) throw new Error(`expected one styles-*.css in ${dir}/assets, found ${built.length}`);
const shell = readFileSync(shellPath, "utf8");
const fixed = shell.replace(/\/assets\/styles-[^"]+\.css/g, `/assets/${built[0]}`);
if (!fixed.includes(`/assets/${built[0]}`)) throw new Error("_shell.html links no stylesheet");
writeFileSync(shellPath, fixed);
console.log(`_shell.html now links /assets/${built[0]}`);
