// Drives the real Self-hosted LiveSync plug-in in a throwaway Obsidian instance
// for the benchmark. Run from the official repo (so its runner and node_modules
// resolve), with tsx:
//
//   cd ~/code/ref/obsidian-livesync && npx tsx <this file>
//
// Isolation comes from upstream's own E2E runner (@vrtmrz/obsidian-test-session):
// a temporary vault, temporary HOME/XDG directories and its own Electron
// --user-data-dir, a remote-debugging port of our choosing (never 9222), and a
// stale-process pattern unique to that profile. The user's Obsidian and vaults
// are never touched.
//
// Protocol: one JSON command per line on stdin, one JSON reply per line on stdout.
//   {"cmd":"start","liveSync":false}  launch Obsidian with the plug-in configured
//   {"cmd":"fetch"}                    run upstream's "Fetch" ($fetchLocal), wait for it
//   {"cmd":"live","on":true}           switch LiveSync on (or off) and apply it
//   {"cmd":"eval","js":"..."}         evaluate an expression in the renderer
//   {"cmd":"stop"}                     stop Obsidian, keep the vault and profile
//   {"cmd":"quit"}                     stop and dispose everything (unless KEEP=1)
//
// Environment: BENCH_DB (database), BENCH_ROOT (where the vault and profile go),
// LSYNC_URL/LSYNC_USER/LSYNC_PASSWORD (dev CouchDB), SCOPE_IGNORE_REGEX,
// BASE_IGNORE_REGEX, OBSIDIAN_BINARY, OBSIDIAN_CLI, E2E_OBSIDIAN_REMOTE_DEBUGGING_PORT.
import { createInterface } from "node:readline";
import { resolve } from "node:path";

const REF = process.cwd();
const imp = (p: string) => import(resolve(REF, p));
// The generic package directly: the runner's wrapper fixes the temporary root.
const { createTemporaryVault } = await imp("node_modules/@vrtmrz/obsidian-test-session/dist/index.js");
const { startObsidianLiveSyncSession } = await imp("test/e2e-obsidian/runner/session.ts");
const { withObsidianPage } = await imp("test/e2e-obsidian/runner/ui.ts");
const { createE2eCouchDbPluginData, createE2eObsidianDeviceLocalState } = await imp(
    "test/e2e-obsidian/runner/liveSyncWorkflow.ts"
);

const env = process.env;
const port = Number(env.E2E_OBSIDIAN_REMOTE_DEBUGGING_PORT ?? 9333);
if (port === 9222) throw new Error("port 9222 belongs to the user's Obsidian");
env.E2E_OBSIDIAN_REMOTE_DEBUGGING_PORT = String(port);
if (!/^http:\/\/(127\.0\.0\.1|localhost):8411/.test(env.LSYNC_URL ?? "")) {
    throw new Error("refusing: the plug-in benchmark runs only against the dev container on 127.0.0.1:8411");
}

let vault: any;
let session: any;
const reply = (o: unknown) => process.stdout.write(JSON.stringify(o) + "\n");
const evalJs = async (js: string) =>
    await withObsidianPage(port, async (page: any) => await page.evaluate(`(async()=>{${js}})()`));

function pluginData(liveSync: boolean) {
    return createE2eCouchDbPluginData(
        { uri: env.LSYNC_URL, username: env.LSYNC_USER, password: env.LSYNC_PASSWORD, dbName: env.BENCH_DB },
        {
            liveSync,
            syncOnStart: false,
            // The phone's folder scope, the same pattern the official CLI gets.
            syncIgnoreRegEx: `${env.SCOPE_IGNORE_REGEX}|[]|${env.BASE_IGNORE_REGEX}`,
            // Keep the plug-in's defaults otherwise (they match the remote's PREFERRED tweaks).
        }
    );
}

async function start(liveSync: boolean) {
    const fresh = !vault;
    if (fresh) {
        vault = await createTemporaryVault({
            prefix: "lsync-bench-plugin-",
            temporaryRoot: env.BENCH_ROOT,
            pluginIds: ["obsidian-livesync"],
            idPrefix: "lsync-bench",
        });
    }
    // Upstream's default arguments plus --ozone-platform=headless: no window on the
    // user's desktop (there is no X server here, and Wayland would show it).
    env.E2E_OBSIDIAN_ARGS = [
        "--no-sandbox",
        "--disable-gpu",
        "--disable-software-rasterizer",
        "--ozone-platform=headless",
        `--user-data-dir=${vault.userDataPath}`,
        `--remote-debugging-port=${port}`,
        `obsidian://open?path=${encodeURIComponent(vault.path)}`,
    ].join(" ");
    const t0 = Date.now();
    session = await startObsidianLiveSyncSession({
        binary: env.OBSIDIAN_BINARY,
        cliBinary: env.OBSIDIAN_CLI,
        vault,
        startupGraceMs: 1000,
        // On a restart, keep the plug-in's own data.json (it has the fetched state).
        pluginData: fresh ? pluginData(liveSync) : undefined,
        localStorageEntries: createE2eObsidianDeviceLocalState(vault.name),
    });
    await waitCoreReady();
    const launchMs = Date.now() - t0;
    if (!fresh) await setLive(liveSync);
    return {
        pid: session.app.process.pid,
        launch_ms: launchMs,
        vault: vault.path,
        profile: vault.statePath,
        userData: vault.userDataPath,
    };
}

// As upstream's waitForLiveSyncCoreReady, over CDP instead of obsidian-cli.
async function waitCoreReady(timeoutMs = 120000) {
    const deadline = Date.now() + timeoutMs;
    let last: unknown;
    while (Date.now() < deadline) {
        try {
            last = await evalJs(
                "const core=app.plugins.plugins['obsidian-livesync']?.core;" +
                    "if(!core) return false;" +
                    "return core.services.database.isDatabaseReady() && core.services.appLifecycle.isReady();"
            );
            if (last === true) return;
        } catch (e) {
            last = e; // the renderer reloads while the plug-in is enabled
        }
        await new Promise((r) => setTimeout(r, 250));
    }
    throw new Error(`LiveSync core not ready: ${String(last)}`);
}

async function setLive(on: boolean) {
    // As upstream's received-change-readiness scenario does it.
    return await evalJs(
        "const core=app.plugins.plugins['obsidian-livesync'].core;" +
            `await core.services.setting.applyExternalSettings({liveSync:${on}},true);` +
            "await core.services.control.applySettings();" +
            (on
                ? "const r=await core.services.replication.startContinuous({trigger:'daemon',interaction:{kind:'forbidden'}});" +
                  "return JSON.stringify(r);"
                : "return 'off';")
    );
}

const rl = createInterface({ input: process.stdin });
for await (const line of rl) {
    if (!line.trim()) continue;
    let msg: any;
    try {
        msg = JSON.parse(line);
        if (msg.cmd === "start") reply({ ok: true, ...(await start(!!msg.liveSync)) });
        else if (msg.cmd === "fetch") {
            const t0 = Date.now();
            await evalJs("await app.plugins.plugins['obsidian-livesync'].core.serviceModules.rebuilder.$fetchLocal(false, true);");
            reply({ ok: true, fetch_ms: Date.now() - t0 });
        } else if (msg.cmd === "live") reply({ ok: true, liveSync: await setLive(!!msg.on) });
        else if (msg.cmd === "eval") reply({ ok: true, result: await evalJs(msg.js) });
        else if (msg.cmd === "stop") {
            await session?.app.stop();
            session = undefined;
            reply({ ok: true });
        } else if (msg.cmd === "quit") {
            await session?.app.stop();
            if (env.KEEP !== "1") await vault?.dispose();
            reply({ ok: true });
            break;
        } else reply({ ok: false, error: `unknown command ${msg.cmd}` });
    } catch (e: any) {
        reply({ ok: false, error: String(e?.stack ?? e) });
    }
}
process.exit(0);
