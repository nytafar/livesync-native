// Upstream decrypts what Rust encrypted. Opt-in, outside the gate:
//
//   LSYNC_CRYPTO_EXPORT=$PWD/target/crypto-export.json \
//     cargo test -p livesync-crypto --test upstream_export -- --ignored
//   node tools/golden/crypto-verify.mjs target/crypto-export.json
//
// Every envelope goes through octagonal-wheels' HKDF `decrypt`, every document
// through commonlib's `outgoing` (E2EEAlgorithm "v2", unencrypted content not
// accepted), and must come back equal to what Rust started from.
import { readFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { isDeepStrictEqual } from "node:util";
import { fileURLToPath, pathToFileURL } from "node:url";
import { setGlobalLogFunction } from "octagonal-wheels/common/logger";
import { decrypt as decryptHKDF } from "octagonal-wheels/encryption/hkdf";

const here = dirname(fileURLToPath(import.meta.url));
const pkgDir = join(here, "node_modules", "@vrtmrz", "livesync-commonlib");
const { getConfiguredFunctionsForEncryption } = await import(pathToFileURL(join(pkgDir, "dist", "pouchdb", "encryption.js")).href);
const version = (dir) => JSON.parse(readFileSync(join(dir, "package.json"), "utf8")).version;

setGlobalLogFunction(() => {});
const file = process.argv[2];
if (!file) {
    console.error("usage: node crypto-verify.mjs <export.json>");
    process.exit(2);
}
const x = JSON.parse(readFileSync(file, "utf8"));
const salt = new Uint8Array(Buffer.from(x.pbkdf2Salt, "base64"));
console.log(`commonlib ${version(pkgDir)}, octagonal-wheels ${version(join(here, "node_modules", "octagonal-wheels"))}`);

let failed = 0;
const report = (ok, what, detail = "") => {
    if (!ok) failed++;
    console.log(`${ok ? "PASS" : "FAIL"} ${what}${detail ? ": " + detail : ""}`);
};
for (const [i, e] of x.envelopes.entries()) {
    try {
        const plain = await decryptHKDF(e.envelope, x.passphrase, salt);
        report(plain === e.plaintext, `envelope ${i} (${e.plaintext.length} units)`);
    } catch (err) {
        report(false, `envelope ${i}`, String(err));
    }
}
const { outgoing } = getConfiguredFunctionsForEncryption(x.passphrase, false, false, async () => salt, "v2", false, () => {}, false);
for (const d of x.documents) {
    try {
        const back = await outgoing(structuredClone(d.remote));
        report(isDeepStrictEqual(back, d.local), `document ${d.local._id}`, isDeepStrictEqual(back, d.local) ? "" : JSON.stringify(back));
    } catch (err) {
        report(false, `document ${d.local._id}`, String(err));
    }
}
console.log(failed ? `${failed} failed` : "all passed");
process.exit(failed ? 1 : 0);
