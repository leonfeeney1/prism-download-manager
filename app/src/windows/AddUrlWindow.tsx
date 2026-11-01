import { useEffect, useState } from "react";
import { TextArea, TextField } from "../components/Controls";
import { LogoMark } from "../components/LogoMark";
import { WindowFrame } from "../components/WindowFrame";
import { Icon } from "../design/icons";
import { api } from "../lib/api";
import { t } from "../lib/i18n";
import { isTorrentLink, looksLikeUrl } from "../lib/present";
import { hexEncode } from "../lib/route";
import { readClipboardText, win } from "../lib/win";

export default function AddUrlWindow() {
  const [url, setUrl] = useState("");
  const [auth, setAuth] = useState(false);
  const [user, setUser] = useState("");
  const [pass, setPass] = useState("");
  const [more, setMore] = useState(false);
  const [sources, setSources] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    void readClipboardText().then((text) => {
      const v = text.trim();
      if (looksLikeUrl(v)) setUrl((cur) => cur || v);
    });
  }, []);

  const valid = looksLikeUrl(url) || /^ftps?:\/\//i.test(url.trim());
  const torrent = isTorrentLink(url);

  const submit = async () => {
    if (!valid || busy) return;
    setBusy(true);
    setError("");
    try {
      if (torrent) await api.openWindow("torrent", hexEncode(url.trim()));
      else {
        // Extra addresses of the same file: each one is validated (same size, range
        // support) when the download starts and then serves segments alongside the
        // primary address; a rejected one is simply never used.
        const mirrors = sources
          .split("\n")
          .map((s) => s.trim())
          .filter((s) => s.length > 0);
        await api.addUrl({ url: url.trim(), username: auth ? user : undefined, password: auth ? pass : undefined, mirrors: mirrors.length > 0 ? mirrors : undefined });
      }
      await win.close();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <WindowFrame kind="addurl" title={t("Add URL")} icon={<LogoMark size={18} glass="indigo" />} className="addurl">
      <div className="wbody">
        <div className="field">
          <span className="field__label">{t("Address")}</span>
          <TextField
            value={url}
            onChange={(v) => {
              setUrl(v);
              setError("");
            }}
            label={t("Download address")}
            icon={torrent ? "magnet" : "link"}
            mono
            autoFocus
            placeholder={t("https://… , ftp://… or magnet:?…")}
            invalid={!!error}
            onEnter={submit}
          />
          <span className={`field__hint${error ? " text-danger" : ""}`}>
            {error || (torrent ? t("Torrent — you will choose the files next.") : t("Prism checks the address and shows the Download File dialog."))}
          </span>
        </div>
        <button type="button" className="disclose" aria-expanded={auth} onClick={() => setAuth(!auth)}>
          <Icon name={auth ? "chevron-down" : "chevron-right"} size={12} strokeWidth={2.2} /> {t("Use a user name and password")}
        </button>
        {auth && (
          <div className="row2">
            <TextField value={user} onChange={setUser} label={t("User name")} placeholder={t("User name")} icon="user" />
            <TextField value={pass} onChange={setPass} label={t("Password")} placeholder={t("Password")} type="password" icon="key" onEnter={submit} />
          </div>
        )}
        <button type="button" className="disclose" aria-expanded={more} onClick={() => setMore(!more)}>
          <Icon name={more ? "chevron-down" : "chevron-right"} size={12} strokeWidth={2.2} /> {t("More options")}
        </button>
        {more && (
          <TextArea value={sources} onChange={setSources} label={t("Additional addresses (one per line)")} placeholder={t("https://… , ftp://… or magnet:?…")} rows={3} />
        )}
      </div>
      <footer className="dialog__footer">
        <span className="spacer" />
        <button type="button" className="btn btn--ghost" onClick={() => void win.close()}>
          {t("Cancel")}
        </button>
        <button type="button" className="btn btn--primary" disabled={!valid || busy} onClick={submit}>
          {busy ? t("Checking…") : t("Continue")}
        </button>
      </footer>
    </WindowFrame>
  );
}
