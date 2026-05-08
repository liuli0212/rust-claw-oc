(async () => {
  const allTools = JSON.parse(__allToolsJson);
  const timerCallbacks = new Map();
  const dueTimerIds = [];
  // Keep long waits interruptible so later, shorter timers are not delayed.
  const TIMER_POLL_SLICE_MS = 25;
  let timerPumpPromise = null;
  let rejectTimerFailure = null;
  const timerFailurePromise = new Promise((_, reject) => {
    rejectTimerFailure = reject;
  });

  function formatRuntimeError(err) {
    const message =
      err && (err.message || err.name)
        ? `${err.name || "Error"}: ${err.message || ""}`
        : String(err);
    const stack = err && err.stack ? String(err.stack) : "";
    return stack ? `${message}\n${stack}` : message;
  }

  function parseToolResult(raw) {
    const result = JSON.parse(raw);
    if (result && result.__rustyClawToolError) {
      throw new Error(result.__rustyClawToolError);
    }
    return result;
  }

  function stringifyHostValue(value) {
    const normalized = value === undefined ? null : value;
    const serialized = JSON.stringify(normalized);
    return serialized === undefined ? "null" : serialized;
  }

  function enqueueDueTimers(timerIds) {
    for (const timerId of timerIds) {
      if (timerCallbacks.has(timerId) && !dueTimerIds.includes(timerId)) {
        dueTimerIds.push(timerId);
      }
    }
  }

  async function drainDueTimers() {
    while (dueTimerIds.length > 0) {
      const timerId = dueTimerIds.shift();
      const callback = timerCallbacks.get(timerId);
      if (!callback) {
        continue;
      }

      timerCallbacks.delete(timerId);
      try {
        await callback();
      } finally {
        __markTimeoutComplete(timerId);
      }
    }
  }

  function timerCancellation(reason) {
    return {
      __rustyClawTimerCancellation: true,
      reason: reason || "Code mode timer loop was cancelled.",
    };
  }

  async function runTimerPump() {
    while (true) {
      const timerState = JSON.parse(__timerStateJson());
      enqueueDueTimers(timerState.due_timer_ids || []);

      if (dueTimerIds.length > 0) {
        await drainDueTimers();
        continue;
      }

      if ((timerState.pending_timers || 0) === 0) {
        return null;
      }

      const resumeAfterMs = timerState.resume_after_ms ?? 100;
      __waiting_for_timer(resumeAfterMs);

      const waitForMs = Math.min(resumeAfterMs, TIMER_POLL_SLICE_MS);
      const waitResult = JSON.parse(await __wait_for_timer(waitForMs));
      if (waitResult && waitResult.cancelled) {
        throw timerCancellation(waitResult.reason);
      }
      if (waitResult && waitResult.disconnected) {
        throw timerCancellation("Code mode timer loop lost its host connection.");
      }
    }
  }

  function ensureTimerPump() {
    if (timerPumpPromise) {
      return;
    }

    timerPumpPromise = runTimerPump()
      .catch((err) => {
        rejectTimerFailure(err);
        throw err;
      })
      .finally(() => {
        timerPumpPromise = null;
      });
  }

  async function waitForPendingTimers() {
    if (timerPumpPromise) {
      await timerPumpPromise;
    }

    await runTimerPump();
  }

  const tools = {};
  for (const toolName of allTools) {
    tools[toolName] = async (args) =>
      parseToolResult(
        await __callTool(
          toolName,
          JSON.stringify(args === undefined ? null : args),
        ),
      );
  }

  globalThis.text = (value) => __text(String(value));
  globalThis.notify = (value) => __notify(String(value));
  globalThis.store = (key, value) =>
    __store(String(key), stringifyHostValue(value));
  globalThis.load = (key) => {
    const raw = __load(String(key));
    return raw == null ? undefined : JSON.parse(raw);
  };
  globalThis.exit = (value) => {
    throw { __rustyClawExit: true, value };
  };
  globalThis.flush = (value) => {
    __flush(stringifyHostValue(value));
  };
  globalThis.setTimeout = (callback, delayMs = 0) => {
    if (typeof callback !== "function") {
      throw new Error("setTimeout() requires a function callback");
    }

    const normalizedDelay = Math.max(0, Math.trunc(Number(delayMs ?? 0) || 0));
    const registration = JSON.parse(__setTimeout(normalizedDelay));
    timerCallbacks.set(registration.timer_id, callback);
    if (registration.run_immediately) {
      dueTimerIds.push(registration.timer_id);
    }
    ensureTimerPump();
    return registration.timer_id;
  };
  globalThis.clearTimeout = (timerId) => {
    if (timerId == null) {
      return undefined;
    }

    const normalizedId = String(timerId);
    timerCallbacks.delete(normalizedId);
    __clearTimeout(normalizedId);

    const queueIndex = dueTimerIds.indexOf(normalizedId);
    if (queueIndex >= 0) {
      dueTimerIds.splice(queueIndex, 1);
    }

    return undefined;
  };

  try {
    const userCodePromise = (async () => {
/*__RUSTY_CLAW_USER_CODE__*/
    })();

    // User code may be awaiting a Promise that only a timer callback can settle.
    // Race only timer failures/cancellations; normal completion still comes
    // from user code, then we drain any remaining unawaited timers.
    const result = await Promise.race([userCodePromise, timerFailurePromise]);
    await waitForPendingTimers();

    return JSON.stringify({
      returnValue: result === undefined ? null : result,
    });
  } catch (err) {
    if (err && err.__rustyClawExit) {
      return JSON.stringify({
        returnValue: err.value === undefined ? null : err.value,
      });
    }

    if (err && err.__rustyClawTimerCancellation) {
      return JSON.stringify({ cancellationReason: err.reason });
    }

    return JSON.stringify({ runtimeError: formatRuntimeError(err) });
  }
})()
