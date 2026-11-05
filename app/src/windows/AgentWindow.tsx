import { useState } from "react";
import { TextField } from "../components/Controls";
import { LogoMark } from "../components/LogoMark";
import { WindowFrame } from "../components/WindowFrame";
import { api } from "../lib/api";
import { t } from "../lib/i18n";
import type { AgentOutcome } from "../lib/types";
import { win } from "../lib/win";

export default function AgentWindow() {
  const [request, setRequest] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState<AgentOutcome | null>(null);

  const submit = async () => {
    const text = request.trim();
    if (!text || busy) return;
    setBusy(true);
    setError("");
    setDone(null);
    try {
      const outcome = await api.runAgent(text);
      setDone(outcome);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <WindowFrame kind="agent" title={t("Ask Prism")} icon={<LogoMark size={18} glass="indigo" />} className="addurl">
      <div className="wbody">
        <div className="field">
          <span className="field__label">{t("Ask Prism to find and download a video")}</span>
          <TextField
            value={request}
            onChange={(v) => {
              setRequest(v);
              setError("");
            }}
            label={t("Ask Prism")}
            icon="search"
            autoFocus
            placeholder={t("The latest episode of Severance in 1080p…")}
            invalid={!!error}
            onEnter={submit}
          />
          <span className={`field__hint${error ? " text-danger" : ""}`}>
            {error || t("Plain words: the video, the quality, or a pasted link.")}
          </span>
        </div>
        {done && (
          <div className="field">
            <span className="field__label">{t("Download started")}</span>
            <span className="selectable">{done.title}</span>
          </div>
        )}
      </div>
      <footer className="dialog__footer">
        <span className="spacer" />
        <button type="button" className="btn btn--ghost" onClick={() => void win.close()}>
          {t("Cancel")}
        </button>
        {done ? (
          <button
            type="button"
            className="btn btn--primary"
            onClick={async () => {
              await api.openWindow("details", done.itemId);
              await win.close();
            }}
          >
            {t("Show download")}
          </button>
        ) : (
          <button type="button" className="btn btn--primary" disabled={!request.trim() || busy} onClick={submit}>
            {busy ? t("Searching…") : t("Ask Prism")}
          </button>
        )}
      </footer>
    </WindowFrame>
  );
}
