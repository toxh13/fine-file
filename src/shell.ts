import { startDrag } from "@crabnebula/tauri-plugin-drag";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { v05, type DriveInfo } from "./api";
import { driveKey } from "./format";

// What goes through Windows itself rather than the page: the Explorer context menu's screen
// point, dragging items out of the window, and what a drop of such a drag back into it is
// read with (the items as dragged, the modifier keys).

// ---- screen points ---------------------------------------------------------------------

/**
 * A point of the page (CSS px, as in MouseEvent.clientX/Y) on the screen, in physical pixels,
 * as the Explorer menu takes it. The page fills the window's client area, so that area's corner
 * plus the point scaled by the device pixel ratio (display scale and page zoom together).
 */
export async function screenPoint(clientX: number, clientY: number): Promise<{ x: number; y: number }> {
  const at = await getCurrentWindow().innerPosition();
  const dpr = window.devicePixelRatio || 1;
  return { x: Math.round(at.x + clientX * dpr), y: Math.round(at.y + clientY * dpr) };
}

/** A point Windows gave in physical pixels of the page (a drop) as CSS px of the page. */
export function pagePoint(p: { x: number; y: number }): { x: number; y: number } {
  const dpr = window.devicePixelRatio || 1;
  return { x: p.x / dpr, y: p.y / dpr };
}

// ---- modifier keys ---------------------------------------------------------------------
// A drop event from Windows does not say which keys are held, and while a drag runs the page
// gets no key or mouse events. So the drop of a drag started here reads them from Windows as it
// comes (keysNow), and goes by those of the press and the move that started it (kept with the
// drag, see dragOut) only when that cannot be done. One from another program is always a copy
// (App), which is also what the cursor shows over this window.

type Keys = { ctrl: boolean; shift: boolean };

const keys: Keys = { ctrl: false, shift: false };

/** Ctrl and Shift held at this moment, as Windows has them; `fallback` when it cannot tell. */
export function keysNow(fallback: Keys): Promise<Keys> {
  return v05.modifierKeys().catch(() => fallback);
}

/** Follows Ctrl and Shift; returns the function that stops it. */
export function trackModifiers(): () => void {
  const note = (e: KeyboardEvent | MouseEvent) => {
    keys.ctrl = e.ctrlKey;
    keys.shift = e.shiftKey;
  };
  const clear = () => {
    keys.ctrl = false;
    keys.shift = false;
  };
  const kinds = ["keydown", "keyup", "mousedown", "mousemove", "mouseup"] as const;
  for (const k of kinds) window.addEventListener(k, note, true);
  window.addEventListener("blur", clear);
  return () => {
    for (const k of kinds) window.removeEventListener(k, note, true);
    window.removeEventListener("blur", clear);
  };
}

// ---- dragging out ----------------------------------------------------------------------

/** How far (CSS px) the pointer moves with the main button held before the items are dragged. */
const DRAG_START = 6;

/**
 * Watches a press of the main button on a row or tile. Moved DRAG_START px with the button held,
 * it calls `drag` (once); let go before that, `click` (a selection that waits for the release, so
 * a press on a selected item can still drag the whole selection).
 */
export function watchPress(ev: React.MouseEvent, drag: () => void, click?: () => void) {
  const x0 = ev.clientX;
  const y0 = ev.clientY;
  const done = () => {
    window.removeEventListener("mousemove", move, true);
    window.removeEventListener("mouseup", up, true);
    window.removeEventListener("blur", done);
  };
  const move = (e: MouseEvent) => {
    // Let go outside the window, unseen.
    if ((e.buttons & 1) === 0) return done();
    if (Math.hypot(e.clientX - x0, e.clientY - y0) < DRAG_START) return;
    done();
    drag();
  };
  const up = (e: MouseEvent) => {
    if (e.button !== 0) return;
    done();
    click?.();
  };
  window.addEventListener("mousemove", move, true);
  window.addEventListener("mouseup", up, true);
  window.addEventListener("blur", done);
}

/** A drag of this window is under way (startDrag has not come back): another one waits for it. */
let dragging = false;

/** How long (ms) after startDrag came back its drop on this window may still be reported. */
const OWN_GRACE = 1000;

/**
 * The last drag started here, for its drop on this window: the items as this app named them and
 * the keys held when it started, until a moment after it is over (`until`, ms).
 */
let own: { paths: string[]; keys: Keys; until: number } | null = null;

/** Whether a drag is over this window now, as the window's drag events last said (App passes them on). */
let over = false;

/** App passes on each drag event of the window: true for one over it, false once it left or dropped. */
export function dragOverWindow(on: boolean) {
  over = on;
}

/**
 * A drop on this window, if it is of the drag started here: the items as they were dragged and
 * the keys held when it started; null for another program's drop. Each drag is taken once.
 *
 * The drop reports the items as the drag plugin resolved them (through junctions and SUBST
 * drives: `C:\Games` moved to another drive comes back as the folder it points to), so the paths
 * dragged are used instead. A drop of as many items, that is; any other is not this drag's. A
 * drag let go on another window is not waited for at all (dragOut), so a drop from another
 * program soon after it is not taken for it.
 */
export function takeOwnDrop(dropped: string[]): { paths: string[]; keys: Keys } | null {
  const o = own;
  if (!o || Date.now() > o.until || o.paths.length !== dropped.length) return null;
  own = null;
  return { paths: o.paths, keys: o.keys };
}

/** Kinds of drive (DriveInfo.kind) whose items the drag can hand to Windows by their path. */
const LOCAL_KINDS = new Set(["fixed", "removable", "cdrom", "ram"]);

/** Path length (UTF-16 units) past which the drag plugin keeps a path as `\\?\C:\…`. */
const DRAG_MAX = 256;

const DRAG_NETWORK = "네트워크 위치·네트워크 드라이브의 항목은 끌어 놓을 수 없어요. 복사·잘라내기와 붙여넣기를 써 주세요";
const DRAG_LONG = `경로가 ${DRAG_MAX}자보다 길어서 끌어 놓을 수 없어요. 복사·잘라내기와 붙여넣기를 써 주세요`;
const DRAG_NAME =
  "이름이 점(.)이나 공백으로 끝나거나, CON·NUL 같은 장치 이름이거나, ? * 같은 글자가 든 항목은 끌어 놓을 수 없어요. 복사·잘라내기와 붙여넣기를 써 주세요";

/** A name Windows keeps for a device: CON, "con.txt", "COM1 .log" (as the drag plugin's dunce reads them). */
function deviceName(name: string): boolean {
  const dot = name.lastIndexOf(".");
  const stem = dot > 0 ? name.slice(0, dot) : name;
  return /^(con|prn|aux|nul|com[1-9]|lpt[1-9])$/i.test(stem.replace(/[. ]+$/, ""));
}

/** A name only a `\\?\` path can carry: ends in a dot or space, a device name, a character Win32 paths refuse. */
function oddName(name: string): boolean {
  return (
    name.length === 0 ||
    name.length > 255 ||
    /[. ]$/.test(name) ||
    /[\x00-\x1f<>:"\/|?*]/.test(name) ||
    deviceName(name)
  );
}

/**
 * Why `paths` cannot be dragged out, or null when they can. The drag plugin resolves each path
 * and hands it to the shell, which cannot read a `\\?\` one, and on the UI thread it then
 * fails in a way that takes the whole app down. Paths stay `\\?\` there when they are on a
 * network share (also a mapped drive), longer than DRAG_MAX, or have a name Win32 paths cannot
 * carry (the Windows menu refuses names ending in a dot or space too, shellmenu.rs). Judged on the
 * paths as this app names them: a junction on the way can still make the resolved path longer.
 */
export function dragRefusal(paths: string[], drives: DriveInfo[]): string | null {
  for (const path of paths) {
    const name = path.replace(/\\+$/, "").split("\\").pop() || path;
    if (!/^[A-Za-z]:\\/.test(path)) return `${DRAG_NETWORK}: ${name}`;
    const d = drives.find((x) => driveKey(x.letter) === driveKey(path));
    if (!d || !LOCAL_KINDS.has(d.kind)) return `${DRAG_NETWORK}: ${name}`;
    if (path.length > DRAG_MAX) return `${DRAG_LONG}: ${name}`;
    if (path.slice(3).split("\\").filter(Boolean).some(oddName)) return `${DRAG_NAME}: ${name}`;
  }
  return null;
}

/**
 * Drags `paths` out of the window as files, for Explorer and other programs (and this window's
 * own folders). Always as a copy: a move would let the program they are dropped on delete the
 * originals, outside this app's checks (a move inside this window is this app's own job, with
 * Shift, see App's drop). The caller checks them with dragRefusal first. Resolves when the drag
 * is over.
 */
export async function dragOut(paths: string[], first: { name: string; is_dir: boolean }): Promise<void> {
  if (dragging || !paths.length) return;
  dragging = true;
  // Until it is over, any drop on this window is this drag's.
  const mine = { paths: [...paths], keys: { ...keys }, until: Infinity };
  own = mine;
  try {
    await startDrag(
      { item: paths, icon: dragImage(first.name, first.is_dir, paths.length), mode: "copy" },
      // Let go nowhere (Esc, or over a window that takes no files), or on another window (the
      // drag left this one, and that event came long before): no drop is coming here. Over this
      // window, its drop event may still be on the way (OWN_GRACE). Read as a string: the plugin
      // sends "Cancel", its types say "Cancelled".
      (r) => {
        if ((String(r.result) !== "Dropped" || !over) && own === mine) own = null;
      },
    );
  } catch (err) {
    if (own === mine) own = null;
    throw err;
  } finally {
    dragging = false;
    mine.until = Date.now() + OWN_GRACE;
  }
}

/** Space left of and above the badge, so the pointer (the image's top left corner) does not cover it. */
const ICON_OFF = 14;
const ICON_H = 24;
const ICON_TEXT = 220;
const ICON_FONT = '12px "Segoe UI Variable", "Segoe UI", "Malgun Gothic", sans-serif';

/**
 * The picture beside the pointer while items are dragged: a badge with the first item's name and
 * how many more there are, as a PNG data URL (the drag plugin takes those, or a file path). Drawn
 * at the screen's pixel density. No pure black: Windows takes black for see-through in drag images.
 */
function dragImage(name: string, dir: boolean, count: number): string {
  const c = document.createElement("canvas");
  const g = c.getContext("2d");
  // Without a picture the drag still runs, with Windows' own.
  if (!g) return "";
  const dpr = Math.max(1, window.devicePixelRatio || 1);
  const label = count > 1 ? `${name} 외 ${(count - 1).toLocaleString()}개` : name;
  g.font = ICON_FONT;
  let text = label;
  if (g.measureText(text).width > ICON_TEXT) {
    while (text.length > 1 && g.measureText(`${text}…`).width > ICON_TEXT) text = text.slice(0, -1);
    text = `${text}…`;
  }
  const tw = Math.ceil(g.measureText(text).width);
  const w = ICON_OFF + 8 + 14 + 6 + tw + 10;
  const h = ICON_OFF + ICON_H;
  c.width = Math.ceil(w * dpr);
  c.height = Math.ceil(h * dpr);
  g.scale(dpr, dpr);
  g.font = ICON_FONT;
  g.fillStyle = "#2f6fde";
  g.beginPath();
  g.roundRect(ICON_OFF, ICON_OFF, w - ICON_OFF, ICON_H, 6);
  g.fill();
  // The folder or page shape of the list's icons.
  const gx = ICON_OFF + 8;
  const gy = ICON_OFF + 5;
  g.fillStyle = dir ? "#e8b33f" : "#eef2f8";
  g.beginPath();
  if (dir) {
    g.moveTo(gx, gy + 2);
    g.lineTo(gx + 5, gy + 2);
    g.lineTo(gx + 6.5, gy);
    g.lineTo(gx + 14, gy);
    g.lineTo(gx + 14, gy + 13);
    g.lineTo(gx, gy + 13);
  } else {
    g.moveTo(gx + 2, gy - 1);
    g.lineTo(gx + 9, gy - 1);
    g.lineTo(gx + 12, gy + 3);
    g.lineTo(gx + 12, gy + 15);
    g.lineTo(gx + 2, gy + 15);
  }
  g.closePath();
  g.fill();
  g.fillStyle = "#ffffff";
  g.textBaseline = "middle";
  g.fillText(text, gx + 14 + 6, ICON_OFF + ICON_H / 2 + 0.5);
  return c.toDataURL("image/png");
}
