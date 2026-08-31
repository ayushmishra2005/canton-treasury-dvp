import { createRequire } from "node:module";
import { mkdir, writeFile } from "node:fs/promises";
import { dirname } from "node:path";

const require = createRequire(new URL("../zama/package.json", import.meta.url));
const { Wallet } = require("ethers");

const [path, passphrase] = process.argv.slice(2);
if (!path || !passphrase) {
  throw new Error("usage: write-relayer-keystore.mjs <path> <passphrase>");
}
const wallet = Wallet.createRandom();
const parsed = JSON.parse(await wallet.encrypt(passphrase));
if (parsed.Crypto && !parsed.crypto) {
  parsed.crypto = parsed.Crypto;
  delete parsed.Crypto;
}
delete parsed["x-ethers"];
await mkdir(dirname(path), { recursive: true });
await writeFile(path, JSON.stringify(parsed));
console.log("wrote " + path);
