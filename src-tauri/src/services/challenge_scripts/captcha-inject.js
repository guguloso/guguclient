/**
 * @fileoverview Inline Arkose FunCaptcha widget injection.
 *
 * Loaded as raw text by `captcha/utils.ts`, prepended with a preamble
 * that assigns `globalThis.__lbxCaptchaInject` with the runtime
 * config, and executed inside the captcha popup via
 * `webContents.executeJavaScript`.
 *
 * The popup loads `https://www.roblox.com/` first so this script runs
 * with a real roblox.com origin (cookies, CSP, fetch credentials).
 * The script then wipes the homepage and replaces it with a minimal
 * Arkose container.
 *
 * Completion is signalled by navigating the popup to
 * `${donePrefix}?token=...` (or `?error=captcha_failed`); the main
 * process's `will-navigate` handler intercepts that URL, extracts the
 * token, and hands it back to the caller of
 * {@link CaptchaService.openCaptchaWindow}.
 *
 * Kept as a `.js` file so editors syntax-highlight it instead of
 * treating it as a template string.
 */

(() => {
    const { dataExchangeBlob, arkoseScriptUrl, donePrefix } =
        globalThis.__lbxCaptchaInject;

    // Stop in-flight requests from the homepage and clear every timer
    // Roblox's own scripts scheduled. Without this, those scripts keep
    // running concurrently with the Arkose widget and frequently navigate
    // the popup or rewrite the DOM out from under us.
    window.stop();
    const maxId = setTimeout(() => undefined, 0);
    for (let i = 0; i <= maxId; i++) {
        clearTimeout(i);
        clearInterval(i);
    }

    // Replace the Roblox homepage with a minimal captcha shell. The
    // `#arkose-container` div is what `enforcement.setConfig` mounts the
    // widget into below.
    document.head.innerHTML =
        "<style>body{margin:0;display:flex;align-items:center;justify-content:center;min-height:100vh;background:#1a1a2e;font-family:system-ui,sans-serif;color:#e0e0e0}#status{text-align:center;padding:20px}#arkose-container{width:100%}</style>";
    document.body.innerHTML =
        '<div id="status">Loading captcha...</div><div id="arkose-container"></div>';

    // Arkose's "verifying browser" probe inspects `navigator.userAgentData`
    // and refuses to load if it doesn't see the "Google Chrome" brand.
    // Electron ships Chromium without that brand by default, so we patch
    // the proxy to inject it (matching real Chrome's shape).
    // biome-ignore lint/performance/useTopLevelRegex: One-shot script, perf irrelevant
    const m = navigator.userAgent.match(/Chrome\/(\d+)\.(\S+)/);
    if (m) {
        const major = m[1];
        const full = `${m[1]}.${m[2]}`;
        const brands = [
            { brand: "Chromium", version: major },
            { brand: "Google Chrome", version: major },
            { brand: "Not=A?Brand", version: "24" },
        ];
        const fullVersionList = [
            { brand: "Chromium", version: full },
            { brand: "Google Chrome", version: full },
            { brand: "Not=A?Brand", version: "24.0.0.0" },
        ];
        if (navigator.userAgentData) {
            const orig = navigator.userAgentData;
            Object.defineProperty(navigator, "userAgentData", {
                value: new Proxy(orig, {
                    get(target, prop) {
                        if (prop === "brands") {
                            return brands;
                        }
                        if (prop === "getHighEntropyValues") {
                            return (hints) =>
                                target.getHighEntropyValues
                                    .call(target, hints)
                                    .then((v) => {
                                        v.brands = brands;
                                        v.fullVersionList = fullVersionList;
                                        return v;
                                    });
                        }
                        const val = Reflect.get(target, prop);
                        return typeof val === "function"
                            ? val.bind(target)
                            : val;
                    },
                }),
                configurable: true,
            });
        }
        // Arkose also checks for the existence of `window.chrome.runtime`.
        // Electron's stub object is missing it; an empty placeholder is
        // enough to pass the probe.
        if (!window.chrome) {
            window.chrome = {};
        }
        if (!window.chrome.runtime) {
            window.chrome.runtime = {};
        }
    }

    // Arkose loads via a script tag with `data-callback="<global name>"`;
    // when ready, Arkose calls the named global with an `enforcement`
    // handle. This is the public Arkose integration contract â€” the
    // function name must match the `data-callback` attribute set below.
    window.onCaptchaReady = (enforcement) => {
        document.getElementById("status").style.display = "none";
        enforcement.setConfig({
            selector: "#arkose-container",
            mode: "inline",
            data: { blob: dataExchangeBlob },
            onReady() {
                // No-op
            },
            onCompleted(response) {
                // Signal success to the main process by navigating to the
                // sentinel URL with the token. The main process's
                // `will-navigate` handler intercepts this and resolves the
                // open promise with the token.
                window.location.href = `${donePrefix}?token=${encodeURIComponent(response.token)}`;
            },
            onError() {
                // Same channel as the success path, but with an error
                // marker so the main process resolves with `null`.
                window.location.href = `${donePrefix}?error=captcha_failed`;
            },
        });
        enforcement.run();
    };

    const s = document.createElement("script");
    s.src = arkoseScriptUrl;
    s.setAttribute("data-callback", "onCaptchaReady");
    s.async = true;
    document.head.appendChild(s);
})();
