import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { Entry, Risk } from "./api";
import { formatDate, formatSize, typeLabel } from "./format";
import { watchPress } from "./shell";
import { focusTakenByConflict } from "./Transfers";

export type SortKey = "name" | "size" | "files" | "modified" | "type" | "location" | "change";

/** A column: one the list sorts by, or "risk" (jev's 위험도, which comes in a while after and is not sorted by). */
export type ColumnKey = SortKey | "risk";

/** A row's "위험도" cell: jev's answer, "asking" while it is on its way, null for nothing to show. */
export type RiskCell = Risk | "asking" | null;

const RISK_LABEL: Record<Risk["level"], string> = { safe: "안전", caution: "주의", danger: "위험" };

/** A row's "변화" cell: its change since the scan before, as shown and explained. */
export interface ChangeCell {
  text: string;
  tone: "grown" | "shrunk" | "new" | "same" | "unknown";
  title: string;
}

export interface Column {
  key: ColumnKey;
  label: string;
  width: string;
  align?: "right";
}

const ROW = 28;

interface Props {
  entries: Entry[];
  /** Changes only when another folder or search is shown; the list then scrolls back to the top. */
  resetKey: string;
  columns: Column[];
  sort: { key: SortKey; desc: boolean };
  onSort: (key: SortKey) => void;
  selected: Set<string>;
  focus: number;
  onSelect: (index: number, mode: "single" | "toggle" | "range") => void;
  onActivate: (entry: Entry) => void;
  onContextMenu: (e: React.MouseEvent, index: number) => void;
  renaming: string | null;
  onRenameDone: (entry: Entry, name: string | null) => void;
  /** Size that 100% of the size bar stands for (current folder total). */
  barTotal: number;
  sized: boolean;
  emptyText: string;
  /** Middle click on a folder (open it in a new tab). */
  onMiddleClick?: (entry: Entry) => void;
  /** The pointer moved with the button held on a row: drag the items out (see Explorer). */
  onDragOut?: (entry: Entry) => void;
  /** Folder rows take dropped files (they carry data-path, which App's drop looks for). */
  dropFolders?: boolean;
  /** The "변화" column's cell for a row. */
  change?: (entry: Entry) => ChangeCell | undefined;
  /** The "위험도" column's cell for a row. */
  risk?: (entry: Entry) => RiskCell;
  /** The rows drawn now (from `first`, up to but not including `last`), scrolled into view or about to be. */
  onRange?: (first: number, last: number) => void;
}

/**
 * A press of the main button on a row or tile. A press on an item already selected keeps the
 * selection until the button comes up (a plain press when several are selected, a Ctrl press that
 * would unselect it), so the selection can be dragged; the click then selects as before. Moved
 * with the button held, the items are dragged out.
 */
export function pressItem(
  ev: React.MouseEvent,
  index: number,
  selected: boolean,
  selectedCount: number,
  onSelect: (index: number, mode: "single" | "toggle" | "range") => void,
  drag?: () => void,
) {
  const mode = ev.shiftKey ? "range" : ev.ctrlKey ? "toggle" : "single";
  const wait = selected && (mode === "toggle" || (mode === "single" && selectedCount > 1));
  if (!wait) onSelect(index, mode);
  if (drag) watchPress(ev, drag, wait ? () => onSelect(index, mode) : undefined);
  else if (wait) onSelect(index, mode);
}

export function FileTable(p: Props) {
  const scroller = useRef<HTMLDivElement>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const [height, setHeight] = useState(600);
  const renameAt = useMemo(
    () => (p.renaming === null ? -1 : p.entries.findIndex((e) => e.path === p.renaming)),
    [p.entries, p.renaming],
  );

  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setHeight(el.clientHeight));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const reveal = (i: number) => {
    const el = scroller.current;
    if (!el || i < 0) return;
    const top = i * ROW;
    if (top < el.scrollTop) el.scrollTop = top;
    else if (top + ROW > el.scrollTop + el.clientHeight) el.scrollTop = top + ROW - el.clientHeight;
  };

  // The path last scrolled to. Another folder or search starts over.
  const revealed = useRef<string | null>(null);

  // Back to the top only for another folder or search, not on every reload or re-sort.
  useEffect(() => {
    revealed.current = null;
    if (scroller.current) scroller.current.scrollTop = 0;
  }, [p.resetKey]);

  // Keep the focused row visible when the focus moves to another item (keyboard, click, the
  // next row after a trash), not when a live reload or re-run only moves the focused item to
  // another row: that would pull the view back to it while the user scrolls away.
  const focusPath = p.entries[p.focus]?.path ?? null;
  useEffect(() => {
    if (focusPath === null || focusPath === revealed.current) return;
    revealed.current = focusPath;
    reveal(p.focus);
  }, [focusPath]);

  // F2 on a selected row that is scrolled away: bring it into view.
  useEffect(() => reveal(renameAt), [p.renaming]);

  const template = p.columns.map((c) => c.width).join(" ");
  const first = Math.max(0, Math.floor(scrollTop / ROW) - 8);
  const last = Math.min(p.entries.length, Math.ceil((scrollTop + height) / ROW) + 8);
  const shown: number[] = [];
  for (let i = first; i < last; i++) shown.push(i);
  // The row being renamed stays mounted while scrolled away, so the typed name is not lost.
  if (renameAt >= 0 && (renameAt < first || renameAt >= last)) shown.push(renameAt);

  // What is in sight, for what is asked about the rows shown (jev's 위험도).
  const onRange = p.onRange;
  useEffect(() => onRange?.(first, last), [onRange, first, last]);

  return (
    <div className="table">
      <div className="thead" style={{ gridTemplateColumns: template }}>
        {p.columns.map((c) => {
          const key = c.key;
          return key === "risk" ? (
            <div
              key={key}
              className="th static"
              title={
                "위험도 (TypeSafe jev): 이 항목을 지워도 되는지 jev가 본 판정이에요 (안전·주의·위험).\n" +
                "· 참고용이에요. 아무것도 지우거나 고르지 않아요\n" +
                "· 경로·이름·크기·날짜 같은 정보만 보내요 (파일 내용은 보내지 않아요)\n" +
                "· 내 jev API 키로 물어서, 요금은 jev 조건대로 키 주인에게 청구돼요\n" +
                '· 툴바의 "위험도"로 켜고 꺼요. 이 열로는 정렬하지 않아요'
              }
            >
              {c.label}
            </div>
          ) : (
            <button key={key} className={`th ${c.align === "right" ? "right" : ""}`} onClick={() => p.onSort(key)}>
              {c.label}
              {p.sort.key === key && <span className="arrow">{p.sort.desc ? "▼" : "▲"}</span>}
            </button>
          );
        })}
      </div>
      <div className="tbody" ref={scroller} onScroll={(e) => setScrollTop(e.currentTarget.scrollTop)}>
        {p.entries.length === 0 && <div className="empty">{p.emptyText}</div>}
        <div style={{ height: p.entries.length * ROW, position: "relative" }}>
          {shown.map((i) => {
            const e = p.entries[i];
            const sel = p.selected.has(e.path);
            return (
              <div
                key={e.path}
                className={`tr ${sel ? "sel" : ""} ${i === p.focus ? "focus" : ""} ${e.hidden || e.system ? "dim" : ""}`}
                style={{ top: i * ROW, gridTemplateColumns: template }}
                data-path={p.dropFolders && e.is_dir ? e.path : undefined}
                onMouseDown={(ev) => {
                  if (ev.button === 1 && e.is_dir && p.onMiddleClick) {
                    // Not the browser's autoscroll.
                    ev.preventDefault();
                    p.onMiddleClick(e);
                  }
                  if (ev.button !== 0) return;
                  const drag = p.onDragOut;
                  pressItem(ev, i, sel, p.selected.size, p.onSelect, drag && (() => drag(e)));
                }}
                onDoubleClick={() => p.onActivate(e)}
                onContextMenu={(ev) => p.onContextMenu(ev, i)}
              >
                {p.columns.map((c) => (
                  <Cell key={c.key} col={c} e={e} {...p} />
                ))}
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}

function Cell({ col, e, barTotal, sized, renaming, onRenameDone, change, risk }: { col: Column; e: Entry } & Props) {
  switch (col.key) {
    case "name":
      return (
        <div className="td name" title={e.path}>
          <span className={`icon ${e.is_dir ? "dir" : "file"} ${e.reparse ? "link" : ""}`} />
          {renaming === e.path ? (
            <RenameBox entry={e} onDone={onRenameDone} />
          ) : (
            <span className="label">{e.name}</span>
          )}
        </div>
      );
    case "size": {
      const known = sized || !e.is_dir;
      const pct = barTotal > 0 && known ? Math.min(100, (e.size / barTotal) * 100) : 0;
      return (
        <div className="td size right">
          <span className="bar" style={{ width: `${pct}%` }} />
          <span className="num">{known ? formatSize(e.size) : "—"}</span>
        </div>
      );
    }
    case "files":
      return <div className="td right muted">{e.is_dir && sized ? e.files.toLocaleString() : ""}</div>;
    case "modified":
      return <div className="td muted">{formatDate(e.modified)}</div>;
    case "type":
      return <div className="td muted">{typeLabel(e.name, e.is_dir)}</div>;
    case "location":
      return (
        <div className="td muted" title={e.path}>
          {e.path.slice(0, Math.max(0, e.path.length - e.name.length - 1))}
        </div>
      );
    case "change": {
      const c = change?.(e);
      return (
        <div className={`td right change ${c?.tone ?? "unknown"}`} title={c?.title}>
          {c?.text ?? ""}
        </div>
      );
    }
    case "risk": {
      const r = risk?.(e) ?? null;
      if (r === "asking") return <div className="td risk muted" title="jev에 묻는 중이에요">…</div>;
      if (!r) return <div className="td risk" />;
      const regen = r.regenerable !== null ? ` · 지워도 프로그램이 다시 만들 확률 ${Math.round(r.regenerable * 100)}%` : "";
      return (
        <div className="td risk" title={`jev 위험도 (참고용) · 신뢰도 ${Math.round(r.confidence * 100)}%${regen}`}>
          <span className={`risk-badge ${r.level}`}>{RISK_LABEL[r.level] ?? r.level}</span>
        </div>
      );
    }
  }
}

/** The name of a row, open for editing (also used by the tree). */
export function RenameBox({ entry, onDone }: { entry: Entry; onDone: (e: Entry, name: string | null) => void }) {
  const ref = useRef<HTMLInputElement>(null);
  // Enter or Escape closes the box, and the blur that follows must not submit a second time.
  const done = useRef(false);
  const finish = (name: string | null) => {
    if (done.current) return;
    done.current = true;
    onDone(entry, name);
  };
  // Only when the box opens: a reload hands in a fresh Entry, and re-selecting then would
  // overwrite what the user is typing.
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.focus();
    // Select the name without the extension, like Explorer.
    const dot = entry.is_dir ? -1 : entry.name.lastIndexOf(".");
    el.setSelectionRange(0, dot > 0 ? dot : entry.name.length);
  }, [entry.path]);
  return (
    <input
      ref={ref}
      className="rename"
      defaultValue={entry.name}
      // Mouse use inside the box is text editing; it must not reach the row (select, open, menu).
      onMouseDown={(ev) => ev.stopPropagation()}
      onDoubleClick={(ev) => ev.stopPropagation()}
      onContextMenu={(ev) => ev.stopPropagation()}
      onKeyDown={(ev) => {
        ev.stopPropagation();
        if (ev.key === "Enter") finish(ev.currentTarget.value);
        if (ev.key === "Escape") finish(null);
      }}
      // The name-conflict dialog takes the focus for a while and gives it back: still typing.
      onBlur={(ev) => focusTakenByConflict(ev) || finish(ev.currentTarget.value)}
    />
  );
}
