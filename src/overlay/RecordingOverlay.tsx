import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import React, { useEffect, useLayoutEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  CancelIcon,
  MicrophoneIcon,
  TranscriptionIcon,
} from "../components/icons";
import "./RecordingOverlay.css";
import { commands, events } from "@/bindings";
import type {
  StreamPhase,
  StreamPhaseEvent,
  StreamTextEvent,
  StreamWorkKind,
} from "@/bindings";
import i18n, { syncLanguageFromSettings } from "@/i18n";
import { getLanguageDirection } from "@/lib/utils/rtl";

type OverlayState =
  | "recording"
  | "streaming"
  | "transcribing"
  | "processing"
  | "memory-tip";

// Number of reactive bars in the waveform (the simple, smoothed style shared by
// every overlay form). Mic levels arrive as 16 FFT buckets; we take the first N.
const WAVE_BARS = 9;

type ShortcutKey = {
  label: string;
  side?: "L" | "R";
};

const shortcutKey = (rawKey: string): ShortcutKey => {
  const normalized = rawKey.trim().toLowerCase();
  const side = normalized.endsWith("_right")
    ? "R"
    : normalized.endsWith("_left")
      ? "L"
      : undefined;
  const key = normalized.replace(/_(left|right)$/, "");
  const labels: Record<string, string> = {
    alt: "Alt",
    command: "⌘",
    control: "Ctrl",
    ctrl: "Ctrl",
    enter: "↵",
    escape: "Esc",
    meta: "⌘",
    option: "⌥",
    return: "↵",
    shift: "⇧",
    space: "Space",
    super: "⌘",
  };

  return { label: labels[key] ?? key.toUpperCase(), side };
};

const shortcutKeys = (binding: string): ShortcutKey[] =>
  binding.split("+").filter(Boolean).map(shortcutKey);

const RecordingOverlay: React.FC = () => {
  const { t } = useTranslation();
  const [isVisible, setIsVisible] = useState(false);
  const [state, setState] = useState<OverlayState>("recording");
  const [levels, setLevels] = useState<number[]>(Array(WAVE_BARS).fill(0));
  const [streamText, setStreamText] = useState<StreamTextEvent>({
    committed: "",
    tentative: "",
  });
  const [phase, setPhase] = useState<StreamPhase>("listening");
  const [workKind, setWorkKind] = useState<StreamWorkKind>("transcribing");
  const [elapsed, setElapsed] = useState(0);
  // Bumped on each new streaming session so the Live card remounts fresh (replays
  // the pop-in, and never animates in from the previous panel's open size).
  const [session, setSession] = useState(0);
  // Overlay placement (top vs bottom of the screen). The Live panel grows downward
  // from a top overlay (oldest line under the pill) and upward from a bottom one.
  const [position, setPosition] = useState<"top" | "bottom">("bottom");
  const [memoryShortcut, setMemoryShortcut] = useState<ShortcutKey[]>([]);
  // True once live text overflows the cap. A top overlay fades its top edge only
  // while overflowing, so the resting first line stays crisp flush under the pill.
  const [overflowing, setOverflowing] = useState(false);

  const smoothedLevelsRef = useRef<number[]>(Array(16).fill(0));
  // Live-text scroll-back: the text region "sticks" to the newest line while the
  // user is at the bottom; if they scroll up to read history, auto-follow pauses
  // until they scroll back down.
  const capRef = useRef<HTMLDivElement>(null);
  const pinnedRef = useRef(true);
  const direction = getLanguageDirection(i18n.language);

  useEffect(() => {
    let disposed = false;
    const unlistenFunctions: UnlistenFn[] = [];

    const registerListener = async (
      listener: Promise<UnlistenFn>,
    ): Promise<void> => {
      try {
        const unlisten = await listener;
        if (disposed) {
          unlisten();
        } else {
          unlistenFunctions.push(unlisten);
        }
      } catch (error) {
        console.error("Failed to register overlay event listener:", error);
      }
    };

    void registerListener(
      listen("show-overlay", async (event) => {
        await syncLanguageFromSettings();
        // The Live panel flows downward from a top overlay and upward from a
        // bottom one; read the placement so the layout can flip to match.
        try {
          const settings = await commands.getAppSettings();
          if (settings.status === "ok") {
            setPosition(
              settings.data.overlay_position === "top" ? "top" : "bottom",
            );
            setMemoryShortcut(
              shortcutKeys(
                settings.data.bindings?.transcribe?.current_binding ?? "",
              ),
            );
          }
        } catch {
          // Keep the previous/default placement if settings can't be read.
        }
        if (disposed) return;

        const overlayState = event.payload as OverlayState;
        setState(overlayState);
        if (overlayState === "recording" || overlayState === "streaming") {
          setStreamText({ committed: "", tentative: "" });
        }
        if (overlayState === "streaming") {
          setPhase("listening");
          setWorkKind("transcribing");
          setElapsed(0);
          setSession((s) => s + 1); // remount the card fresh for this session
        }
        setIsVisible(true);
      }),
    );

    void registerListener(
      listen("hide-overlay", () => {
        setIsVisible(false);
      }),
    );

    void registerListener(
      listen<number[]>("mic-level", (event) => {
        const newLevels = event.payload as number[];
        // Exponential smoothing across the 16 buckets, then take the first N
        // bars for the shared waveform.
        const smoothed = smoothedLevelsRef.current.map((prev, i) => {
          const target = newLevels[i] || 0;
          return prev * 0.7 + target * 0.3;
        });
        smoothedLevelsRef.current = smoothed;
        setLevels(smoothed.slice(0, WAVE_BARS));
      }),
    );

    void registerListener(
      events.streamTextEvent.listen((event) => {
        setStreamText(event.payload);
      }),
    );

    void registerListener(
      events.streamPhaseEvent.listen((event) => {
        const payload: StreamPhaseEvent = event.payload;
        setPhase(payload.phase);
        if (payload.kind) setWorkKind(payload.kind);
      }),
    );

    return () => {
      disposed = true;
      unlistenFunctions.splice(0).forEach((unlisten) => unlisten());
    };
  }, []);

  // Elapsed timer while the Live overlay is visible.
  useEffect(() => {
    if (state !== "streaming" || !isVisible) return;
    const id = setInterval(() => setElapsed((e) => e + 1), 1000);
    return () => clearInterval(id);
  }, [state, isVisible]);

  // Stick to the bottom as text streams in — but only while pinned, so a user who
  // has scrolled up to read history isn't yanked back down by the next chunk.
  useLayoutEffect(() => {
    const el = capRef.current;
    if (!el) return;
    // Fade the top edge only once text actually overflows the cap.
    setOverflowing(el.scrollHeight > el.clientHeight + 1);
    if (pinnedRef.current) el.scrollTop = el.scrollHeight;
  }, [streamText]);

  // Each fresh streaming session starts pinned to the bottom, fade cleared.
  useEffect(() => {
    pinnedRef.current = true;
    setOverflowing(false);
  }, [session]);

  // Re-pin when the user is within ~a line of the bottom; unpin otherwise.
  const handleStreamScroll = () => {
    const el = capRef.current;
    if (!el) return;
    pinnedRef.current = el.scrollHeight - el.scrollTop - el.clientHeight <= 16;
  };

  const fmtTime = (s: number) =>
    `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;

  // ---- Shared building blocks (one visual language for every overlay form) ----
  const waveform = (
    <div className="swave">
      {levels.map((v, i) => (
        <i
          key={i}
          style={{
            height: `${Math.max(3, Math.min(18, 3 + Math.pow(v, 0.7) * 15))}px`,
          }}
        />
      ))}
    </div>
  );

  const cancelBtn = (
    <button
      className="sx"
      aria-label="cancel"
      onClick={() => commands.cancelOperation()}
    >
      <svg viewBox="0 0 16 16" aria-hidden="true">
        <path
          d="M4 4 L12 12 M12 4 L4 12"
          stroke="currentColor"
          strokeWidth="1.6"
          strokeLinecap="round"
        />
      </svg>
    </button>
  );

  // dot (left) | waveform (center) | timer + cancel (right) — same structure for
  // pill & panel, so the Live morph is a pure width change.
  const listeningRow = (showTimer: boolean, showCancel: boolean) => (
    <div className="sbase">
      <div className="sbase-l">
        <span className="sdot" />
      </div>
      {waveform}
      <div className="sbase-r">
        {showTimer && <span className="stimer">{fmtTime(elapsed)}</span>}
        {showCancel && cancelBtn}
      </div>
    </div>
  );

  // spinner (left) | label (center) | cancel (right) — same 3-zone grid as the
  // listening row, so the label is centered.
  const workingRow = (label: string, showCancel: boolean) => (
    <div className="sbase">
      <div className="sbase-l">
        <span className="sspinner" />
      </div>
      <span className="swork-label">{label}</span>
      <div className="sbase-r">{showCancel && cancelBtn}</div>
    </div>
  );

  if (state === "memory-tip") {
    return (
      <div dir={direction} className="memory-tip-stage">
        <div
          className={`memory-tip-layout ${isVisible ? "fade-in" : ""}`}
          role="status"
          aria-live="polite"
        >
          <div className="memory-tip-message">
            <span className="memory-tip-icon" aria-hidden="true">
              <MicrophoneIcon width={19} height={19} />
            </span>
            <span className="memory-tip-text">{t("overlay.memoryTip")}</span>
          </div>
          {memoryShortcut.length > 0 && (
            <div className="memory-tip-shortcut" dir="ltr">
              {memoryShortcut.map((key, index) => (
                <React.Fragment
                  key={`${key.label}-${key.side ?? "any"}-${index}`}
                >
                  {index > 0 && (
                    <span className="memory-tip-plus" aria-hidden="true">
                      +
                    </span>
                  )}
                  <kbd className="memory-tip-key">
                    <span>{key.label}</span>
                    {key.side && <small aria-hidden="true">{key.side}</small>}
                  </kbd>
                </React.Fragment>
              ))}
            </div>
          )}
        </div>
      </div>
    );
  }

  // ---- Live overlay: a pill that sculpts open into a panel ----
  if (state === "streaming") {
    const hasText =
      streamText.committed.length > 0 || streamText.tentative.length > 0;
    const working = phase === "working";
    // Keep the panel open whenever there's text — even while finalizing — so the
    // transcript stays put under a working spinner instead of collapsing and
    // squishing the text mid-stream. Only fall back to the small working pill
    // when there was no text to preserve.
    const open = hasText;
    const collapsed = working && !hasText;

    return (
      <div dir={direction} className={`ov-stage ${position}`}>
        <div
          key={session}
          className={`scard ${open ? "open" : ""} ${collapsed ? "working" : ""} ${
            isVisible ? "" : "leaving"
          }`}
        >
          <div className="stext">
            <div className="stext-clip">
              <div
                className={`stext-cap ${overflowing ? "overflowing" : ""}`}
                ref={capRef}
                onScroll={handleStreamScroll}
              >
                <p>
                  <span className="committed">
                    {streamText.committed ? streamText.committed + " " : ""}
                  </span>
                  <span className="tentative">{streamText.tentative}</span>
                  {/* Drop the blinking caret once finalizing — it's no longer
                      capturing, and a static spinner conveys the work. */}
                  {!working && <span className="scaret" />}
                </p>
              </div>
            </div>
          </div>
          {working
            ? workingRow(
                workKind === "polishing"
                  ? t("overlay.processing")
                  : t("overlay.transcribing"),
                true,
              )
            : listeningRow(open, true)}
        </div>
      </div>
    );
  }

  // ---- Classic compact overlay (the pre-0.9 recording indicator) ----
  // Keep the current state machine and cancellation behavior, but render the
  // original 172×36 black pill, icons, and pink level bars.
  return (
    <div
      dir={direction}
      className={`recording-overlay ${isVisible ? "fade-in" : ""}`}
    >
      <div className="overlay-left">
        {state === "recording" ? <MicrophoneIcon /> : <TranscriptionIcon />}
      </div>

      <div className="overlay-middle">
        {state === "recording" && (
          <div className="bars-container">
            {levels.map((level, index) => (
              <div
                key={index}
                className="bar"
                style={{
                  height: `${Math.min(20, 4 + Math.pow(level, 0.7) * 16)}px`,
                  opacity: Math.max(0.2, level * 1.7),
                }}
              />
            ))}
          </div>
        )}
        {state === "transcribing" && (
          <div className="transcribing-text">{t("overlay.transcribing")}</div>
        )}
        {state === "processing" && (
          <div className="transcribing-text">{t("overlay.processing")}</div>
        )}
      </div>

      <div className="overlay-right">
        <button
          type="button"
          className="cancel-button"
          aria-label="cancel"
          onClick={() => commands.cancelOperation()}
        >
          <CancelIcon />
        </button>
      </div>
    </div>
  );
};

export default RecordingOverlay;
