/**
 * @fileoverview Captcha replay popup orchestration.
 *
 * Loaded as raw text by `captcha/utils.ts`, prepended with a preamble
 * that assigns `globalThis.__lbxCaptchaReplay` with the runtime config,
 * and executed inside the popup via `webContents.executeJavaScript`.
 *
 * Used for **mutating** captcha-protected requests. The popup must
 * re-issue the original request from inside its own browser session,
 * because Roblox binds the captcha challenge to the session that asked
 * for it â€” feeding a challenge id from the main process's session into
 * an Arkose widget running in this popup's session causes Roblox to
 * reject the solution with "Challenge failed to authorize request".
 *
 * Returns either:
 *   - a serialized `{ status, statusText, url, headers, bodyText }` for
 *     the terminal HTTP response Roblox returned, or
 *   - an `{ __lunarReplayError: true, message }` sentinel if
 *     anything in the orchestration threw.
 *
 * The TS caller (`CaptchaService.openCaptchaReplayWindow`) awaits
 * `executeJavaScript`, distinguishes the two return shapes via
 * `isSerializedPopupResponse`, and synthesizes a fallback 500 response
 * for the error case so RoZod always sees a real `Response`.
 *
 * Kept as a `.js` file so editors syntax-highlight it instead of
 * treating it as a template string.
 */

(async () => {
    const {
        replayRequest,
        initialChallenge,
        arkoseScriptUrlTemplate,
        challengeContinueUrl,
        funCaptchaPublicKeys,
        actionTypeToFunCaptchaKey,
        fallbackFunCaptchaActionType,
    } = globalThis.__lbxCaptchaReplay;

    // Accumulate step logs so the main process can persist them after the
    // popup (and its DevTools console) close.
    const replayDebugLog = [];
    const logStep = (message) => {
        replayDebugLog.push(message);
        console.info(message);
    };

    /**
     * Resolves the Arkose widget URL for a captcha's action type. Roblox mints
     * the data-exchange blob for the action's specific public key, so loading
     * the widget under the wrong key makes the solved token fail verification.
     */
    const resolveArkoseScriptUrl = (actionType) => {
        const actionTypeKey =
            (actionType && actionTypeToFunCaptchaKey[actionType]) ||
            fallbackFunCaptchaActionType;
        const publicKey =
            funCaptchaPublicKeys[actionTypeKey] ||
            funCaptchaPublicKeys[fallbackFunCaptchaActionType];
        if (!publicKey) {
            throw new Error(
                `No FunCaptcha public key found for action type key "${actionTypeKey}"`
            );
        }
        return arkoseScriptUrlTemplate.replace("{publicKey}", publicKey);
    };

    // ------------------------------------------------------------------
    // Phase 0: silent browser-fingerprint patches
    //
    // Applied while Roblox's homepage is still intact so Arkose's
    // "verifying browser" probe â€” which inspects the parent window
    // when the widget mounts later â€” sees a normal, fully-initialized
    // Chrome session. The previous flow wiped the page first, which
    // made the verifier loop indefinitely and eventually fail with
    // captcha_failed.
    // ------------------------------------------------------------------

    // navigator.userAgentData â€” Electron is missing the "Google Chrome"
    // brand on its Client Hints; Arkose treats that as non-Chrome and
    // refuses to load. Patch the proxy to inject the brand into both
    // `brands` and `getHighEntropyValues()`'s `fullVersionList`.
    try {
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
        }
    } catch {
        console.warn("Failed to patch navigator.userAgentData");
    }

    // navigator.webdriver â€” Selenium / Puppeteer / etc. set this to
    // `true`. Real browsers leave it `undefined`; Arkose flags any
    // non-undefined value as automation.
    try {
        Object.defineProperty(navigator, "webdriver", {
            get() {
                return;
            },
            configurable: true,
        });
    } catch {
        console.warn("Failed to patch navigator.webdriver property");
    }

    // window.chrome shape â€” Arkose probes for `chrome.app`,
    // `chrome.runtime`, `chrome.csi`, and `chrome.loadTimes`. Electron
    // ships an incomplete stub; fill in plausible defaults that match
    // what real Chrome exposes.
    try {
        if (!window.chrome) {
            window.chrome = {};
        }
        if (!window.chrome.app) {
            window.chrome.app = {
                isInstalled: false,
                InstallState: {
                    DISABLED: "disabled",
                    INSTALLED: "installed",
                    NOT_INSTALLED: "not_installed",
                },
                RunningState: {
                    CANNOT_RUN: "cannot_run",
                    READY_TO_RUN: "ready_to_run",
                    RUNNING: "running",
                },
                getDetails() {
                    return null;
                },
                getIsInstalled() {
                    return false;
                },
            };
        }
        if (!window.chrome.runtime) {
            window.chrome.runtime = {
                OnInstalledReason: {
                    CHROME_UPDATE: "chrome_update",
                    INSTALL: "install",
                    SHARED_MODULE_UPDATE: "shared_module_update",
                    UPDATE: "update",
                },
                OnRestartRequiredReason: {
                    APP_UPDATE: "app_update",
                    OS_UPDATE: "os_update",
                    PERIODIC: "periodic",
                },
                PlatformArch: {
                    ARM: "arm",
                    ARM64: "arm64",
                    MIPS: "mips",
                    MIPS64: "mips64",
                    X86_32: "x86-32",
                    X86_64: "x86-64",
                },
                PlatformOs: {
                    ANDROID: "android",
                    CROS: "cros",
                    LINUX: "linux",
                    MAC: "mac",
                    OPENBSD: "openbsd",
                    WIN: "win",
                },
                RequestUpdateCheckStatus: {
                    NO_UPDATE: "no_update",
                    THROTTLED: "throttled",
                    UPDATE_AVAILABLE: "update_available",
                },
            };
        }
        if (!window.chrome.csi) {
            window.chrome.csi = () => ({
                onloadT: Date.now(),
                pageT: performance.now(),
                startE: Date.now(),
                tran: 15,
            });
        }
        if (!window.chrome.loadTimes) {
            window.chrome.loadTimes = () => ({
                requestTime: performance.timeOrigin / 1000,
                startLoadTime: performance.timeOrigin / 1000,
                commitLoadTime: performance.timeOrigin / 1000,
                finishDocumentLoadTime: performance.timeOrigin / 1000,
                finishLoadTime: performance.timeOrigin / 1000,
                firstPaintTime: performance.timeOrigin / 1000,
                firstPaintAfterLoadTime: 0,
                navigationType: "Other",
                wasFetchedViaSpdy: true,
                wasNpnNegotiated: true,
                npnNegotiatedProtocol: "h2",
                wasAlternateProtocolAvailable: false,
                connectionInfo: "h2",
            });
        }
    } catch {
        console.warn("Failed to patch window.chrome properties");
    }

    // ------------------------------------------------------------------
    // Phase 1: page-hijack helper, deferred until we actually need a
    // captcha.
    //
    // We delay wiping the homepage so Arkose, when it eventually
    // initializes, snapshots a real fully-loaded roblox.com page rather
    // than a single empty container. The fingerprint patches above run
    // immediately, but the visible DOM stays untouched until we know we
    // need to render the widget.
    // ------------------------------------------------------------------
    let statusEl = null;
    let pageHijacked = false;
    const hijackPage = () => {
        if (pageHijacked) {
            return;
        }
        pageHijacked = true;

        // Stop in-flight requests and clear every timer Roblox's own
        // scripts scheduled, so they can't navigate the popup or rewrite
        // our captcha shell after we install it.
        try {
            window.stop();
        } catch {
            console.warn("Failed to stop page load during hijack");
        }
        try {
            const maxId = setTimeout(() => undefined, 0);
            for (let i = 0; i <= maxId; i++) {
                clearTimeout(i);
                clearInterval(i);
            }
        } catch {
            console.warn("Failed to clear timers during page hijack");
        }

        document.head.innerHTML =
            '<meta charset="utf-8"><style>' +
            "html,body{margin:0;padding:0;background:#1a1a2e;color:#e0e0e0;" +
            "font-family:system-ui,-apple-system,Segoe UI,sans-serif;min-height:100vh}" +
            "body{display:flex;flex-direction:column;align-items:center;" +
            "justify-content:flex-start;padding:24px 16px;box-sizing:border-box}" +
            "#lbx-title{font-size:16px;font-weight:600;margin-bottom:6px;text-align:center}" +
            "#lbx-subtitle{font-size:13px;color:#a0a0b8;margin-bottom:20px;text-align:center;max-width:420px}" +
            "#lbx-status{font-size:13px;color:#a0a0b8;margin-bottom:16px;text-align:center}" +
            "#lbx-arkose{width:100%;max-width:420px}" +
            "</style>";
        document.body.innerHTML =
            '<div id="lbx-title">Roblox Verification Required</div>' +
            '<div id="lbx-subtitle">Roblox has asked you to confirm you are human before this action can continue. Solve the challenge below to proceed.</div>' +
            '<div id="lbx-status">Loading challengeâ€¦</div>' +
            '<div id="lbx-arkose"></div>';

        statusEl = document.getElementById("lbx-status");
    };

    /** Updates the captcha shell status line. No-op before `hijackPage`. */
    const setStatus = (message, hide) => {
        if (!statusEl) {
            return;
        }
        statusEl.textContent = message || "";
        statusEl.style.display = hide ? "none" : "";
    };

    // ------------------------------------------------------------------
    // Phase 2: fetch / response helpers
    //
    // Everything below operates on the popup's own session â€” same
    // origin, same cookies as a normal roblox.com browsing session.
    // ------------------------------------------------------------------

    /**
     * Pulls `rblx-challenge-*` headers off a response into the
     * canonical challenge shape, or returns null if the response isn't
     * a challenge.
     */
    const parseChallenge = (headers) => {
        const challengeId = headers.get("rblx-challenge-id");
        const challengeType = headers.get("rblx-challenge-type");
        const challengeBase64Metadata = headers.get("rblx-challenge-metadata");
        if (!(challengeId && challengeType && challengeBase64Metadata)) {
            return null;
        }
        return { challengeId, challengeType, challengeBase64Metadata };
    };

    /** Best-effort decode of a challenge's base64 metadata for logging. */
    const decodeChallengeMetadata = (challenge) => {
        try {
            return atob(challenge.challengeBase64Metadata);
        } catch {
            return "<undecodable>";
        }
    };

    /** Serializes a fetch Response into the IPC-friendly success shape. */
    const serializeResponse = async (response) => {
        let bodyText = "";
        try {
            bodyText = await response.clone().text();
        } catch {
            console.warn("Failed to read response body as text");
        }
        return {
            status: response.status,
            statusText: response.statusText || "",
            url: response.url || replayRequest.url,
            headers: Array.from(response.headers.entries()),
            bodyText,
        };
    };

    // The original `x-csrf-token` header from the main process. We
    // remember its case-preserving key so re-emitting in `buildInit`
    // doesn't accidentally duplicate the header.
    const csrfHeaderKey = (() => {
        for (const key of Object.keys(replayRequest.headers || {})) {
            if (key.toLowerCase() === "x-csrf-token") {
                return key;
            }
        }
        return "x-csrf-token";
    })();

    let currentCsrf = replayRequest.headers?.[csrfHeaderKey] || "";

    /**
     * Builds the fetch `init` for the original request, copying the
     * caller's headers (minus their CSRF, which we manage separately
     * to handle rotation), the current CSRF token, and any extra
     * headers passed in (used to add the solved `rblx-challenge-*`
     * headers on the retry). Body is reconstructed from RoZod's
     * serialized form.
     */
    const buildInit = (extraHeaders) => {
        const headers = new Headers();
        for (const [name, value] of Object.entries(
            replayRequest.headers || {}
        )) {
            if (name.toLowerCase() === "x-csrf-token") {
                continue;
            }
            headers.set(name, value);
        }
        if (currentCsrf) {
            headers.set("x-csrf-token", currentCsrf);
        }
        for (const [name, value] of Object.entries(extraHeaders || {})) {
            headers.set(name, value);
        }

        const init = {
            method: replayRequest.method || "GET",
            credentials: "include",
            headers,
            mode: "cors",
            redirect: "follow",
        };

        if (replayRequest.body) {
            if (replayRequest.body.type === "text") {
                init.body = replayRequest.body.value;
            } else if (replayRequest.body.type === "form-data") {
                const formData = new FormData();
                for (const field of replayRequest.body.fields) {
                    formData.append(field.name, field.value);
                }
                // Let the browser regenerate `content-type` so the
                // multipart boundary matches the FormData body.
                headers.delete("content-type");
                init.body = formData;
            }
        }

        return init;
    };

    /**
     * Issues the original request, transparently rotating the CSRF
     * token once if Roblox returns a fresh one. Captcha-protected
     * endpoints frequently rotate CSRF when they return the challenge,
     * so without this the retry would always 403.
     */
    const runOriginalRequest = async (extraHeaders) => {
        let response = await fetch(replayRequest.url, buildInit(extraHeaders));
        const rotatedCsrf = response.headers.get("x-csrf-token");
        if (
            response.status === 403 &&
            rotatedCsrf &&
            rotatedCsrf !== currentCsrf
        ) {
            currentCsrf = rotatedCsrf;
            response = await fetch(replayRequest.url, buildInit(extraHeaders));
        } else if (rotatedCsrf) {
            currentCsrf = rotatedCsrf;
        }
        return response;
    };

    // ------------------------------------------------------------------
    // Phase 3: Arkose FunCaptcha widget
    // ------------------------------------------------------------------

    /**
     * Mounts the Arkose widget into `#lbx-arkose` and resolves with the
     * captcha token once the user solves it. Rejects on any of
     * Arkose's failure callbacks. Safe to call multiple times in a
     * single popup (clears the previous widget's container first).
     */
    const solveArkose = (dataExchangeBlob, arkoseScriptUrl) =>
        new Promise((resolve, reject) => {
            // Wipe the container so a previous widget instance from an
            // earlier loop iteration doesn't render twice.
            const container = document.getElementById("lbx-arkose");
            if (container) {
                container.innerHTML = "";
            }
            setStatus("Waiting for captcha to loadâ€¦");

            let settled = false;
            const settle = (fn, value) => {
                if (settled) {
                    return;
                }
                settled = true;
                fn(value);
            };

            // Arkose's integration contract: the script tag's
            // `data-callback` attribute names a global function Arkose
            // invokes with the enforcement handle.
            window.onLunarCaptchaReady = (enforcement) => {
                try {
                    enforcement.setConfig({
                        selector: "#lbx-arkose",
                        mode: "inline",
                        data: { blob: dataExchangeBlob },
                        onReady() {
                            setStatus("", true);
                        },
                        onShown() {
                            setStatus("", true);
                        },
                        onCompleted(r) {
                            settle(resolve, r?.token);
                        },
                        onSuppress() {
                            settle(reject, new Error("captcha_suppressed"));
                        },
                        onError(err) {
                            settle(
                                reject,
                                new Error(
                                    `captcha_error:${err?.error?.error || "unknown"}`
                                )
                            );
                        },
                        onFailed() {
                            settle(reject, new Error("captcha_failed"));
                        },
                    });
                    enforcement.run();
                } catch (error) {
                    settle(reject, error);
                }
            };

            // If we looped, a previous Arkose script tag is still in the
            // DOM and reusing the global callback name. Remove it before
            // re-injecting so the new tag's load triggers our handler.
            const existing = document.querySelector("script[data-lbx-arkose]");
            if (existing) {
                existing.remove();
            }

            const script = document.createElement("script");
            script.src = arkoseScriptUrl;
            script.setAttribute("data-callback", "onLunarCaptchaReady");
            script.setAttribute("data-lbx-arkose", "1");
            script.async = true;
            script.onerror = () =>
                settle(reject, new Error("arkose_script_load_failed"));
            document.head.appendChild(script);
        });

    // ------------------------------------------------------------------
    // Phase 4: orchestration â€” captcha â†’ continue â†’ retry
    // ------------------------------------------------------------------

    /**
     * Solves a captcha challenge end-to-end:
     *   1. Hijack the page (we know we need a captcha now).
     *   2. Mount Arkose, wait for the user's solution.
     *   3. Best-effort POST to `/challenge/v1/continue` to register
     *      the solution server-side. Several Roblox endpoints
     *      (notably unauthenticated ones like signup) reject this
     *      with 404 and validate the solution inline from the
     *      `rblx-challenge-metadata` header on the retry instead, so
     *      we log non-2xx but never block on it.
     *   4. Re-issue the original request with the solved
     *      `rblx-challenge-*` headers populated.
     *
     * Returns the response of step 4.
     */
    const handleCaptchaChallenge = async (challenge) => {
        let metadata;
        try {
            metadata = JSON.parse(atob(challenge.challengeBase64Metadata));
        } catch {
            throw new Error("Failed to decode captcha challenge metadata");
        }

        hijackPage();

        const arkoseScriptUrl = resolveArkoseScriptUrl(metadata.actionType);
        logStep(
            `[lunar replay] arkose key for actionType=${metadata.actionType} -> ${arkoseScriptUrl}`
        );

        const captchaToken = await solveArkose(
            metadata.dataExchangeBlob,
            arkoseScriptUrl
        );
        if (!captchaToken) {
            throw new Error("Captcha returned an empty token");
        }
        logStep(
            `[lunar replay] arkose solved, token length=${captchaToken.length}`
        );

        setStatus("Registering challenge solutionâ€¦");

        // The shape Roblox expects in both the continue body's
        // `challengeMetadata` field and the retry's
        // `rblx-challenge-metadata` header.
        const solutionMetadata = btoa(
            JSON.stringify({
                unifiedCaptchaId: metadata.unifiedCaptchaId,
                captchaId: metadata.captchaId,
                captchaToken,
                actionType: metadata.actionType,
            })
        );

        const continueBody = JSON.stringify({
            challengeId: challenge.challengeId,
            challengeType: challenge.challengeType,
            challengeMetadata: atob(solutionMetadata),
        });

        const callContinue = (csrfToken) => {
            const headers = new Headers();
            headers.set("content-type", "application/json");
            if (csrfToken) {
                headers.set("x-csrf-token", csrfToken);
            }
            return fetch(challengeContinueUrl, {
                method: "POST",
                credentials: "include",
                headers,
                mode: "cors",
                redirect: "follow",
                body: continueBody,
            });
        };

        try {
            let continueResp = await callContinue(currentCsrf);
            const rotatedCsrf = continueResp.headers.get("x-csrf-token");
            if (rotatedCsrf && rotatedCsrf !== currentCsrf) {
                currentCsrf = rotatedCsrf;
                if (continueResp.status === 403) {
                    continueResp = await callContinue(currentCsrf);
                }
            }
            const continueText = await continueResp
                .clone()
                .text()
                .catch(() => "");
            logStep(
                `[lunar replay] /challenge/v1/continue -> ${continueResp.status} ${continueText.slice(0, 500)}`
            );
        } catch (continueError) {
            logStep(
                `[lunar replay] /challenge/v1/continue threw: ${continueError instanceof Error ? continueError.message : String(continueError)}`
            );
        }

        setStatus("Submitting verified requestâ€¦");

        return runOriginalRequest({
            "rblx-challenge-id": challenge.challengeId,
            "rblx-challenge-type": challenge.challengeType,
            "rblx-challenge-metadata": solutionMetadata,
        });
    };

    try {
        // Issue the original request first. Roblox returns a fresh
        // captcha challenge bound to *this* popup's session â€” using
        // that one (rather than the main process's `initialChallenge`)
        // is what makes the eventual solution authorize the retry.
        //
        // The page is intentionally NOT hijacked yet â€” Roblox's
        // homepage scripts continue running so Arkose's later
        // parent-window probe sees a fully-initialized real page.
        let response = await runOriginalRequest();
        logStep(
            `[lunar replay] initial re-issue -> ${response.status}, challenge=${!!parseChallenge(response.headers)}`
        );
        let nextChallenge = parseChallenge(response.headers);
        if (nextChallenge) {
            logStep(
                `[lunar replay] re-issue challenge: type=${nextChallenge.challengeType} metadata=${decodeChallengeMetadata(nextChallenge)}`
            );
        }

        // Fallback: if the popup's request unexpectedly didn't return
        // a fresh challenge but did fail (e.g. transient backend hiccup
        // ate the headers), use the main process's observed challenge
        // so the user still gets a chance to solve and the transaction
        // can land. Solution may not authorize in that path, but it's
        // strictly better than failing silently with no UI.
        if (!(nextChallenge || response.ok)) {
            nextChallenge = initialChallenge;
        }

        // Loop in case Roblox chains a follow-up captcha after the
        // first solve. Cap at 3 to avoid spinning forever on a backend
        // that keeps re-issuing.
        let iterations = 0;
        while (
            nextChallenge &&
            nextChallenge.challengeType === "captcha" &&
            iterations < 3
        ) {
            iterations++;
            response = await handleCaptchaChallenge(nextChallenge);
            logStep(
                `[lunar replay] post-solve retry -> ${response.status}, challenge=${!!parseChallenge(response.headers)}`
            );
            nextChallenge = parseChallenge(response.headers);
        }

        if (nextChallenge) {
            // A non-captcha follow-up challenge (e.g. 2FA, rostile,
            // proofofwork). We can't solve it here, so surface the response
            // as-is and let RoZod / the caller decide what to do with it.
            logStep(
                `[lunar replay] unsolved challenge handed off: type=${nextChallenge.challengeType} metadata=${decodeChallengeMetadata(nextChallenge)}`
            );
            setStatus("Additional verification required; handing offâ€¦");
        } else {
            setStatus("Verification complete.");
        }

        const serialized = await serializeResponse(response);
        return { ...serialized, debugLog: replayDebugLog };
    } catch (error) {
        // Any uncaught failure becomes the sentinel error shape the TS
        // caller knows how to detect and convert into a 500 response.
        return {
            __lunarReplayError: true,
            message: error instanceof Error ? error.message : String(error),
            debugLog: replayDebugLog,
        };
    }
})();
