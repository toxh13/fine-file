import { useEffect, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { adminCanRead, adminError, ensureAdmin } from "./AdminSession";
import { ops, v04, type DriveInfo, type Entry, type PreviewInfo } from "./api";
import { Icon } from "./CommandBar";
import { folderTitle, formatDate, formatSize, parentOf, pathKey, shown, typeLabel } from "./format";
import { Glyph } from "./GridView";
import "./components.css";

// ---- the details pane (상세 정보): what an item is, where it is, and a preview of it. It is a tab of
// the side pane, whose head (the tabs, the close button) is Explorer's. ----

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

/** What the preview shows: the last entry whose preview arrived. */
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

/** Kinds the preview fills the rest of the pane with. */
const FILLS = new Set<PreviewInfo["kind"]>(["image", "video", "audio", "pdf", "text"]);

const COPY_TITLE = "전체 경로를 글자로 클립보드에 복사해요";

export interface PreviewProps {
  entry: Entry | null;
  /** Several items selected (list, icons): a summary of them instead of one item's details. */
  selection?: Entry[];
  /**
   * The size of the folder the entry (or the selection) is in, when it is known (a scanned folder):
   * the share of it that it takes is shown.
   */
  parentSize?: number;
  /** The location's copy button: the full paths go to the clipboard as text. */
  onCopyPaths: (items: Entry[]) => void;
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

/** "42%", "0.4%", "0.1% 미만". */
function shareText(part: number, whole: number): string {
  const r = (part / whole) * 100;
  if (r >= 10) return `${Math.min(100, r).toFixed(0)}%`;
  if (r >= 0.1) return `${r.toFixed(1)}%`;
  return r > 0 ? "0.1% 미만" : "0%";
}

/** The share of its folder an item (or a selection in one folder) takes, with a bar. */
function Share({ size, parent, folder }: { size: number; parent: number; folder: string }) {
  const name = shown(folderTitle(folder));
  const where = /^[A-Za-z]:$/.test(name) ? `${name} 드라이브` : `${name} 폴더`;
  return (
    <>
      <dt>차지 비율</dt>
      <dd className="details-share" title={`${formatSize(size)} / ${formatSize(parent)}`}>
        <span>
          {where}의 {shareText(size, parent)}
        </span>
        <span className="details-share-bar" aria-hidden="true">
          <span style={{ width: `${Math.min(100, (size / parent) * 100)}%` }} />
        </span>
      </dd>
    </>
  );
}

/** Where an item is (the folder it is in), selectable, with a button that copies the full paths. */
function Location({ text, onCopy }: { text: string; onCopy: () => void }) {
  return (
    <>
      <dt>위치</dt>
      <dd className="details-loc">
        <span className="preview-path">{shown(text)}</span>
        <button type="button" className="details-copy" aria-label="경로 복사" title={COPY_TITLE} onClick={onCopy}>
          <Icon.copyPath />
        </button>
      </dd>
    </>
  );
}

export function Preview({ entry, selection, parentSize, onCopyPaths, sized, adminFolder, drives }: PreviewProps) {
  const [loaded, setLoaded] = useState<Shown | null>(null);
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
      setLoaded(null);
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
      if (!stale) setLoaded({ key: entryKey(e), entry: e, info, error });
    }, DEBOUNCE);
    return () => {
      stale = true;
      clearTimeout(t);
    };
  }, [key]);

  // An online-only cloud file is not loaded at all: <img>, <video> or the PDF viewer would have
  // the sync app download the whole of it.
  const kind: PreviewInfo["kind"] | null = !loaded
    ? null
    : loaded.info && !loaded.info.online_only && broken !== loaded.key
      ? loaded.info.kind
      : "none";

  // The shell thumbnail only where it adds something: folders, files the page cannot show (online-
  // only ones too, which get only a thumbnail Windows already has) and audio (album art). It is the
  // icon at the top and their preview. Images, video, PDFs and text are shown in full below, so
  // the top keeps their type's icon: no second read of the file (a slow one on a network drive),
  // and the thumbnail workers stay free for the icon view.
  const thumbKey = loaded && (kind === "none" || kind === "audio") ? loaded.key : null;
  useEffect(() => {
    if (!loaded || thumbKey === null) return;
    const path = loaded.entry.path;
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
  const thumbDone = !!thumb && thumb.key === thumbKey;
  const thumbUrl = thumbDone ? thumb.url : null;

  const loading = entry !== null && loaded?.key !== key;
  const fresh = loaded && loaded.key === key ? loaded : null;

  // What the admin session read for the entry shown, once it is in.
  const elevated = loaded && adminRead?.key === loaded.key ? adminRead.info : null;
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

  if (selection && selection.length > 1) {
    return <Summary items={selection} sized={sized} parentSize={parentSize} onCopyPaths={onCopyPaths} />;
  }

  if (!entry) return <div className="preview-empty">항목을 고르면 여기에 상세 정보가 보여요</div>;

  let sizeText = "—";
  // preview_info reads the size from disk, which is newer than the index's.
  if (!entry.is_dir) sizeText = formatSize((fresh && elevated?.size) || fresh?.info?.size || entry.size);
  const folderKnown = entry.is_dir && (sized ?? (entry.size > 0 || entry.files > 0));
  if (folderKnown) sizeText = formatSize(entry.size);
  const attrs = [entry.hidden && "숨김", entry.system && "시스템", entry.reparse && "링크(정션)"].filter(Boolean).join(", ");
  const folder = parentOf(entry.path);
  const sizeKnown = !entry.is_dir || folderKnown;

  // Files only: a folder's thumbnail is the icon at the top. Another file's preview stays, dimmed,
  // while this one's loads.
  const preview = loaded && !loaded.entry.is_dir && !entry.is_dir ? loaded : null;
  const fill = !!preview && (!!elevated || (kind !== null && FILLS.has(kind)));

  return (
    <div className="details-scroll">
      <div className="details-top">
        <span className="details-icon">
          {fresh && thumbUrl ? <img src={thumbUrl} alt="" draggable={false} /> : <Glyph e={entry} />}
        </span>
        <span className="details-title" title={shown(entry.path)}>
          {shown(entry.name)}
        </span>
      </div>

      <dl className="preview-meta">
        <dt>종류</dt>
        <dd title={fresh?.info?.mime ?? undefined}>{shown(typeLabel(entry.name, entry.is_dir))}</dd>
        <dt>크기</dt>
        <dd>{sizeText}</dd>
        {folderKnown && (
          <>
            <dt>파일 수</dt>
            <dd>{entry.files.toLocaleString()}개</dd>
          </>
        )}
        <dt>수정한 날짜</dt>
        <dd>{formatDate(entry.modified) || "—"}</dd>
        <Location text={folder ?? entry.path} onCopy={() => onCopyPaths([entry])} />
        {attrs && (
          <>
            <dt>속성</dt>
            <dd>{attrs}</dd>
          </>
        )}
        {folder && sizeKnown && !!parentSize && parentSize > 0 && <Share size={entry.size} parent={parentSize} folder={folder} />}
      </dl>

      {preview && (
        <section className={`details-preview${fill ? " fill" : ""}`} aria-label="미리 보기">
          <h4 className="details-sub">미리 보기</h4>
          <div className={`preview-body ${loading ? "stale" : ""}`}>
            {elevated ? (
              <AdminBody s={preview} info={elevated} />
            ) : kind ? (
              <Body
                s={preview}
                kind={kind}
                thumb={thumbUrl}
                thumbDone={thumbDone}
                broken={broken === preview.key}
                onBroken={() => setBroken(preview.key)}
              />
            ) : null}
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
                {adminRead?.key === fresh.key && adminRead.error && <p className="preview-note error">{shown(adminRead.error)}</p>}
                <p className="muted">앞부분 256 KB만 읽고, 파일은 바꾸지 않아요.</p>
              </div>
            )}
          </div>
        </section>
      )}
    </div>
  );
}

/**
 * Several items selected: how many, of what, how big together, and where. Folder sizes count only
 * where they are known (a scanned folder, search results).
 */
function Summary({
  items,
  sized,
  parentSize,
  onCopyPaths,
}: {
  items: Entry[];
  sized?: boolean;
  parentSize?: number;
  onCopyPaths: (items: Entry[]) => void;
}) {
  const dirs = items.filter((e) => e.is_dir).length;
  const files = items.length - dirs;
  const known = !!sized || dirs === 0;
  const size = items.reduce((s, e) => s + e.size, 0);
  const fileBytes = items.reduce((s, e) => s + (e.is_dir ? 0 : e.size), 0);
  const inside = items.reduce((s, e) => s + (e.is_dir ? e.files : 1), 0);
  const folders = new Set(items.map((e) => pathKey(parentOf(e.path) ?? e.path)));
  const folder = folders.size === 1 ? parentOf(items[0].path) : null;
  const kinds = [dirs && `폴더 ${dirs.toLocaleString()}개`, files && `파일 ${files.toLocaleString()}개`].filter(Boolean).join(", ");
  return (
    <div className="details-scroll">
      <div className="details-top">
        <span className="details-icon stack">
          <Glyph e={items[1]} />
          <Glyph e={items[0]} />
        </span>
        <span className="details-title">{items.length.toLocaleString()}개 항목</span>
      </div>
      <dl className="preview-meta">
        <dt>종류</dt>
        <dd>{kinds}</dd>
        <dt>합계 크기</dt>
        <dd title={known ? undefined : "폴더 크기는 스캔한 드라이브에서만 보여요"}>
          {known ? formatSize(size) : files ? `${formatSize(fileBytes)} + 폴더 (크기 모름)` : "알 수 없음"}
        </dd>
        {known && dirs > 0 && (
          <>
            <dt>파일 수</dt>
            <dd>{inside.toLocaleString()}개</dd>
          </>
        )}
        <Location text={folder ?? `여러 폴더 (${folders.size.toLocaleString()}곳)`} onCopy={() => onCopyPaths(items)} />
        {folder && known && !!parentSize && parentSize > 0 && <Share size={size} parent={parentSize} folder={folder} />}
      </dl>
      <p className="details-hint muted">항목을 하나만 고르면 미리 볼 수 있어요</p>
    </div>
  );
}

interface BodyProps {
  s: Shown;
  kind: PreviewInfo["kind"];
  /** Shell thumbnail for "none" and "audio"; null while it loads or when there is none. */
  thumb: string | null;
  /** The thumbnail was asked for and answered (null then means there is none). */
  thumbDone: boolean;
  broken: boolean;
  onBroken: () => void;
}

function Body({ s, kind, thumb, thumbDone, broken, onBroken }: BodyProps) {
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
      return <iframe key={s.key} className="preview-pdf" src={src} title={shown(s.entry.name)} />;
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
    default: {
      const note = s.info?.online_only
        ? "온라인 전용 파일이라 미리 보려면 내려받아야 해요"
        : broken
          ? "이 형식은 미리 보기로 열 수 없어요"
          : null;
      return (
        <div className="preview-media">
          {/* The icon at the top already shows what a file without a thumbnail is. */}
          {thumb ? (
            <img src={thumb} alt="" draggable={false} />
          ) : (
            thumbDone && !note && !s.error && <p className="preview-note">미리 볼 수 있는 내용이 없어요</p>
          )}
          {note && <p className="preview-note">{note}</p>}
          {s.error && <p className="preview-note error">{shown(s.error)}</p>}
        </div>
      );
    }
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
