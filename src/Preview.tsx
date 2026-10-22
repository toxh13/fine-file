import { useEffect, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { adminCanRead, adminError, ensureAdmin } from "./AdminSession";
import { ops, v04, type DriveInfo, type Entry, type PreviewInfo } from "./api";
import { formatDate, formatSize, typeLabel } from "./format";
import { Glyph } from "./GridView";
import "./components.css";

/** Wait for the selection to settle, so arrowing through a folder loads nothing in between. */
const DEBOUNCE = 150;
const THUMB = 256;

/** A preview error that means this account may not read the file (access denied). */
const isDenied = (e: string | null) => e !== null && /\(os error 5\)/.test(e);

/** A file read through the admin session (read-only): its start as text, or as a hex dump. */
interface AdminRead {
  /** entryKey() of the entry it was read for. */
  key: string;
  info: PreviewInfo | null;
  error: string | null;
}

/** What the body shows: the last entry whose preview arrived. */
interface Shown {
  /** entryKey() of the entry it was loaded for. */
  key: string;
  entry: Entry;
  /** Null for a folder, which has no content of its own to show. */
  info: PreviewInfo | null;
  error: string | null;
}

/** A file saved again gets a fresh preview; a folder whose size changes keeps its own. */
const entryKey = (e: Entry) => `${e.path}|${e.modified}`;

export interface PreviewProps {
  entry: Entry | null;
  onClose: () => void;
  /** Folder sizes are known (indexed folder, search results). Left out, a folder shows a size when it has one. */
  sized?: boolean;
  /**
   * The entry is in a folder read with administrator rights: a file with nothing to show the
   * normal way may be read through the admin session too, not only one that was refused.
   */
  adminFolder?: boolean;
  /** This computer's drives: "관리자 권한으로 읽기" is offered only where the admin session can read. */
  drives: DriveInfo[];
}

export function Preview({ entry, onClose, sized, adminFolder, drives }: PreviewProps) {
  const [shown, setShown] = useState<Shown | null>(null);
  // Media the page could not play after all (a codec WebView2 lacks): the shell thumbnail stands in.
  const [broken, setBroken] = useState<string | null>(null);
  const [thumb, setThumb] = useState<{ key: string; url: string | null } | null>(null);
  // Asked for by the user ("관리자 권한으로 읽기"), for one entry; another entry shows its own preview.
  const [adminRead, setAdminRead] = useState<AdminRead | null>(null);
  const [adminBusy, setAdminBusy] = useState<string | null>(null);
  const key = entry ? entryKey(entry) : "";

  // The old preview stays (dimmed) until the new one arrives; an answer for an entry that is no
  // longer selected is dropped.
  useEffect(() => {
    if (!entry) {
      setShown(null);
      return;
    }
    const e = entry;
    let stale = false;
    const t = setTimeout(async () => {
      let info: PreviewInfo | null = null;
      let error: string | null = null;
      if (!e.is_dir) {
        try {
          info = await ops.previewInfo(e.path);
        } catch (err) {
          error = String(err);
        }
      }
      if (!stale) setShown({ key: entryKey(e), entry: e, info, error });
    }, DEBOUNCE);
    return () => {
      stale = true;
      clearTimeout(t);
    };
  }, [key]);

  // An online-only cloud file is not loaded at all: <img>, <video> or the PDF viewer would have
  // the sync app download the whole of it.
  const kind: PreviewInfo["kind"] | null = !shown
    ? null
    : shown.info && !shown.info.online_only && broken !== shown.key
      ? shown.info.kind
      : "none";

  // Folders, files the page cannot show, and audio (album art) get the shell thumbnail.
  const thumbKey = shown && (kind === "none" || kind === "audio") ? shown.key : null;
  useEffect(() => {
    if (!shown || thumbKey === null) return;
    const path = shown.entry.path;
    let stale = false;
    ops
      .thumbnail(path, THUMB)
      .catch(() => null)
      .then((url) => {
        if (!stale) setThumb({ key: thumbKey, url });
      });
    return () => {
      stale = true;
    };
  }, [thumbKey]);
  const thumbUrl = thumb && thumb.key === thumbKey ? thumb.url : null;

  const loading = entry !== null && shown?.key !== key;
  const fresh = shown && shown.key === key ? shown : null;

  // What the admin session read for the entry shown, once it is in.
  const elevated = shown && adminRead?.key === shown.key ? adminRead.info : null;
  // A file this account may not read, or one with nothing to show in a folder read with
  // administrator rights: it can be read through the admin session (read-only, no download of
  // an online-only cloud file). Not on a network place, which the helper cannot read.
  const canElevate =
    !!fresh &&
    !fresh.entry.is_dir &&
    !elevated &&
    adminCanRead(fresh.entry.path, drives) &&
    (isDenied(fresh.error) || (!!adminFolder && kind === "none" && !fresh.info?.online_only));
  const readAsAdmin = async (s: Shown) => {
    setAdminBusy(s.key);
    let r: AdminRead;
    try {
      if (!(await ensureAdmin())) throw new Error("관리자 권한을 받지 못했어요");
      r = { key: s.key, info: await v04.adminRead(s.entry.path), error: null };
    } catch (err) {
      r = { key: s.key, info: null, error: adminError(err) };
    }
    setAdminRead(r);
    setAdminBusy((b) => (b === s.key ? null : b));
  };

  let sizeText = "—";
  // preview_info reads the size from disk, which is newer than the index's.
  if (entry && !entry.is_dir) sizeText = formatSize((fresh && elevated?.size) || fresh?.info?.size || entry.size);
  const folderKnown = !!entry?.is_dir && (sized ?? (entry.size > 0 || entry.files > 0));
  if (entry && folderKnown) sizeText = formatSize(entry.size);
  const attrs = entry
    ? [entry.hidden && "숨김", entry.system && "시스템", entry.reparse && "링크(정션)"].filter(Boolean).join(", ")
    : "";

  return (
    <aside className="preview">
      <div className="preview-head">
        <span className="preview-title" title={entry?.path}>
          {entry?.name ?? "미리 보기"}
        </span>
        <button className="preview-close" title="미리 보기 닫기" onClick={onClose}>
          ✕
        </button>
      </div>

      <div className={`preview-body ${loading ? "stale" : ""}`}>
        {shown && elevated ? (
          <AdminBody s={shown} info={elevated} />
        ) : shown && kind ? (
          <Body s={shown} kind={kind} thumb={thumbUrl} broken={broken === shown.key} onBroken={() => setBroken(shown.key)} />
        ) : entry ? (
          <div className="preview-media">
            <Glyph e={entry} />
          </div>
        ) : (
          <div className="preview-empty">항목을 고르면 여기에 미리 보여요</div>
        )}
        {canElevate && fresh && (
          <div className="preview-admin">
            <p>
              {isDenied(fresh.error)
                ? "이 파일은 지금 권한으로 읽을 수 없어요."
                : "관리자 권한으로 읽은 폴더예요. 관리자 권한으로 파일 앞부분을 읽어 볼 수 있어요."}
            </p>
            <button disabled={adminBusy === fresh.key} onClick={() => readAsAdmin(fresh)}>
              {adminBusy === fresh.key ? "읽는 중…" : "관리자 권한으로 읽기"}
            </button>
            {adminRead?.key === fresh.key && adminRead.error && <p className="preview-note error">{adminRead.error}</p>}
            <p className="muted">앞부분 256 KB만 읽고, 파일은 바꾸지 않아요.</p>
          </div>
        )}
      </div>

      {entry && (
        <dl className="preview-meta">
          <dt>크기</dt>
          <dd>{sizeText}</dd>
          {folderKnown && (
            <>
              <dt>파일</dt>
              <dd>{entry.files.toLocaleString()}개</dd>
            </>
          )}
          <dt>수정한 날짜</dt>
          <dd>{formatDate(entry.modified) || "—"}</dd>
          <dt>종류</dt>
          <dd title={fresh?.info?.mime ?? undefined}>{typeLabel(entry.name, entry.is_dir)}</dd>
          {attrs && (
            <>
              <dt>속성</dt>
              <dd>{attrs}</dd>
            </>
          )}
          <dt>경로</dt>
          <dd className="preview-path">{entry.path}</dd>
        </dl>
      )}
    </aside>
  );
}

interface BodyProps {
  s: Shown;
  kind: PreviewInfo["kind"];
  /** Shell thumbnail for "none" and "audio"; null while it loads or when there is none. */
  thumb: string | null;
  broken: boolean;
  onBroken: () => void;
}

function Body({ s, kind, thumb, broken, onBroken }: BodyProps) {
  // Only image, video, audio and pdf kinds are in the asset scope (preview_info adds them).
  const src = kind === "image" || kind === "video" || kind === "audio" || kind === "pdf" ? convertFileSrc(s.entry.path) : "";
  switch (kind) {
    case "image":
      return (
        <div className="preview-media">
          <img key={s.key} src={src} alt="" draggable={false} onError={onBroken} />
        </div>
      );
    case "video":
      return (
        <div className="preview-media">
          <video key={s.key} src={src} controls preload="metadata" onError={onBroken} />
        </div>
      );
    case "audio":
      return (
        <div className="preview-media audio">
          {thumb ? <img src={thumb} alt="" draggable={false} /> : <Glyph e={s.entry} />}
          <audio key={s.key} src={src} controls preload="metadata" onError={onBroken} />
        </div>
      );
    case "pdf":
      return <iframe key={s.key} className="preview-pdf" src={src} title={s.entry.name} />;
    case "text":
      return (
        <>
          <pre className="preview-text">{s.info?.text ?? ""}</pre>
          <div className="preview-note">
            {s.info?.encoding ?? "텍스트"}
            {s.info?.truncated && " · (앞부분만 표시)"}
          </div>
        </>
      );
    default:
      return (
        <div className="preview-media">
          {thumb ? <img src={thumb} alt="" draggable={false} /> : <Glyph e={s.entry} />}
          {s.info?.online_only && <p className="preview-note">온라인 전용 파일이라 미리 보려면 내려받아야 해요</p>}
          {broken && <p className="preview-note">이 형식은 미리 보기로 열 수 없어요</p>}
          {s.error && <p className="preview-note error">{s.error}</p>}
        </div>
      );
  }
}

/**
 * The start of a file as the admin session read it: text, or a hex dump ("HEX") for a file that
 * is not text. Never media: the page loads nothing of the file itself.
 */
function AdminBody({ s, info }: { s: Shown; info: PreviewInfo }) {
  if (info.kind === "text") {
    const hex = info.encoding === "HEX";
    return (
      <>
        <pre className={`preview-text${hex ? " hex" : ""}`}>{info.text ?? ""}</pre>
        <div className="preview-note">
          관리자 권한으로 읽음 (읽기 전용) · {hex ? "16진수 (텍스트가 아닌 파일)" : (info.encoding ?? "텍스트")}
          {info.truncated && " · (앞부분만 표시)"}
        </div>
      </>
    );
  }
  return (
    <div className="preview-media">
      <Glyph e={s.entry} />
      <p className="preview-note">
        {info.online_only ? "온라인 전용 파일이라 미리 보려면 내려받아야 해요" : "관리자 권한으로도 보여 줄 내용이 없어요"}
      </p>
    </div>
  );
}
