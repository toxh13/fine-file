import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState } from "react";

// ---- the toolbar's second row (the command bar), as Windows 11 Explorer has it ----------

/**
 * Below this width of the command bar (a window of about 900 px, with the sidebar) the less used
 * commands (정리 ▾) fold into "⋯". All of them take about 600 px.
 */
const FOLD_AT = 640;

/** How far the command bar is folded: 0 nothing, then one more group per step (see Explorer). */
export const FOLD_MAX = 3;

/**
 * How far the command bar folds. By its width first (FOLD_AT); then one step more while what is
 * left still does not fit (a large font, a badge). `content` changes when buttons come or go (the
 * view's own ones), so the bar is measured again. Each step lands before the bar is painted.
 */
export function useFold(bar: React.RefObject<HTMLElement | null>, content: string): number {
  const [width, setWidth] = useState(0);
  const [fold, setFold] = useState(1);
  useLayoutEffect(() => {
    const el = bar.current;
    if (!el) return;
    setWidth(el.clientWidth);
    const ro = new ResizeObserver(() => setWidth(el.clientWidth));
    ro.observe(el);
    return () => ro.disconnect();
  }, [bar]);
  useLayoutEffect(() => {
    if (width > 0) setFold(width >= FOLD_AT ? 0 : 1);
  }, [width, content]);
  // After every render: what is shown must fit. The bar's buttons do not shrink, so too many show
  // as content wider than the bar.
  useLayoutEffect(() => {
    const el = bar.current;
    if (el && fold < FOLD_MAX && el.scrollWidth > el.clientWidth + 1) setFold(fold + 1);
  });
  return fold;
}

/** The enabled items of a menu, in order. */
const itemsOf = (menu: HTMLElement | null) =>
  menu ? Array.from(menu.querySelectorAll<HTMLElement>('[role^="menuitem"]:not(:disabled)')) : [];

export interface MenuButtonProps {
  /** What the button shows. */
  label: React.ReactNode;
  className?: string;
  disabled?: boolean;
  /** The button's tooltip, accessible name and description (tip(), aria-label). */
  attrs?: { title?: string; "aria-label"?: string; "aria-describedby"?: string };
  /** The menu's accessible name. */
  name: string;
  /** The tab is shown: a hidden tab's menu closes. */
  active: boolean;
  menuClassName?: string;
  /** The items: buttons with role menuitem, menuitemradio or menuitemcheckbox (and <hr>s). */
  children: React.ReactNode;
}

/**
 * A button that opens a menu under it (보기 ▾, 정리 ▾, ⋯). The keyboard works as in a menu button:
 * Enter or Space (or a click) open it with the focus on the checked (or first) item; ↑ ↓ Home End
 * move, Esc and Tab close it and give the focus back to the button. Choosing an item closes it (the
 * focus goes back to the button when it was chosen with the keyboard); a click anywhere else, the
 * focus going elsewhere on the page (Ctrl+F to the search box, a rename box) or leaving the window
 * closes it too. Keys typed in it stay there, except shortcuts with Ctrl or Alt, F1 (help) and F5
 * (refresh), so the list's keys (Delete, F2, Shift+F10…) never act on the list while it is open.
 */
export function MenuButton(p: MenuButtonProps) {
  const [open, setOpen] = useState(false);
  const button = useRef<HTMLButtonElement>(null);
  const box = useRef<HTMLDivElement>(null);
  const id = useId();
  const shown = open && p.active && !p.disabled;

  // The tab hidden, or the button turned off (a search started): it stays closed.
  useEffect(() => {
    if (!p.active || p.disabled) setOpen(false);
  }, [p.active, p.disabled]);

  const close = useCallback((refocus: boolean) => {
    setOpen(false);
    if (refocus) button.current?.focus();
  }, []);

  // Under the button, lined up with its left edge, or with its right edge where it would not fit;
  // above it in a window too low for it. The focus goes in at once, to the checked item (else the
  // first).
  useLayoutEffect(() => {
    const el = box.current;
    const b = button.current;
    if (!shown || !el || !b) return;
    const r = b.getBoundingClientRect();
    const { width, height } = el.getBoundingClientRect();
    const left = r.left + width <= window.innerWidth - 4 ? r.left : Math.max(4, r.right - width);
    const below = r.bottom + 4;
    const top = below + height <= window.innerHeight - 4 ? below : Math.max(4, r.top - 4 - height);
    el.style.left = `${left}px`;
    el.style.top = `${top}px`;
    const items = itemsOf(el);
    const checked = items.find((i) => i.getAttribute("aria-checked") === "true");
    (checked ?? items[0])?.focus();
    // Only when it opens: the items re-render while it is open (a badge, a check).
  }, [shown]);

  useEffect(() => {
    if (!shown) return;
    const down = (ev: PointerEvent) => {
      const t = ev.target as Node;
      if (box.current?.contains(t) || button.current?.contains(t)) return;
      setOpen(false);
    };
    const leave = () => setOpen(false);
    // Capture: some areas stop their pointer events (the map, the rows being dragged).
    window.addEventListener("pointerdown", down, true);
    window.addEventListener("blur", leave);
    return () => {
      window.removeEventListener("pointerdown", down, true);
      window.removeEventListener("blur", leave);
    };
  }, [shown]);

  const onMenuKey = (ev: React.KeyboardEvent<HTMLDivElement>) => {
    const items = itemsOf(box.current);
    const at = items.indexOf(document.activeElement as HTMLElement);
    const move = (i: number) => items.length && items[(i + items.length) % items.length].focus();
    switch (ev.key) {
      case "ArrowDown":
        move(at + 1);
        break;
      case "ArrowUp":
        move(at < 0 ? items.length - 1 : at - 1);
        break;
      case "Home":
        move(0);
        break;
      case "End":
        move(items.length - 1);
        break;
      case "Escape":
      case "Tab":
        close(true);
        break;
      default:
        // Shortcuts go on to the window (Ctrl+Shift+1, Alt+P), and so do F1 (help) and F5
        // (refresh); Enter and Space click the item (the browser does). Every other key stops
        // here, the other F keys too: F2 (rename) and Shift+F10 (the Windows menu) would act on
        // the list under the menu.
        if (ev.ctrlKey || ev.altKey || ev.metaKey || ev.key === "F1" || ev.key === "F5") return;
        // Kept from the page, as the list does: Shift+F10 would open the browser's own menu.
        if (ev.key === "F10" && ev.shiftKey) ev.preventDefault();
        ev.stopPropagation();
        return;
    }
    ev.preventDefault();
    ev.stopPropagation();
  };

  // The focus went somewhere else on the page (Ctrl+F to the search box, a rename box): it closes.
  // Focus going nowhere (a click on the menu's padding or a disabled item, leaving the window) is
  // left to the pointerdown and blur listeners above.
  const onFocusOut = (ev: React.FocusEvent) => {
    const to = ev.relatedTarget as Node | null;
    if (to && !box.current?.contains(to) && !button.current?.contains(to)) setOpen(false);
  };

  return (
    <>
      <button
        ref={button}
        type="button"
        className={`${p.className ?? "cmd"}${shown ? " on" : ""}`}
        disabled={p.disabled}
        aria-haspopup="menu"
        aria-expanded={shown}
        aria-controls={shown ? id : undefined}
        {...p.attrs}
        // A click closes the other menus (they close on any click); this one opens or closes.
        // ↑ ↓ on the button are left to the list (they blur it and move through the rows), so
        // the focus given back to it after the menu closes takes none of the list's keys.
        onClick={() => setOpen((o) => !o)}
        onBlur={onFocusOut}
      >
        {p.label}
      </button>
      {shown && (
        <div
          ref={box}
          id={id}
          className={`menu cmd-menu ${p.menuClassName ?? ""}`}
          role="menu"
          aria-label={p.name}
          onKeyDown={onMenuKey}
          onBlur={onFocusOut}
          onClick={(ev) => {
            // An item was chosen (a disabled one gets no click): the menu closes, and the focus
            // goes back to the button when it was chosen with the keyboard.
            const item = (ev.target as HTMLElement).closest('[role^="menuitem"]');
            if (item && box.current?.contains(item)) close(ev.detail === 0);
          }}
        >
          {p.children}
        </div>
      )}
    </>
  );
}

/** One row of a command menu: a check for radio and checkbox items, an icon, the label, its key. */
export function MenuRow(p: { check?: boolean; icon?: React.ReactNode; label: React.ReactNode; keys?: string }) {
  return (
    <span className="cmd-menu-row">
      {p.check !== undefined && (
        <span className="cmd-menu-check" aria-hidden="true">
          {p.check ? "✓" : ""}
        </span>
      )}
      {p.icon !== undefined && <span className="cmd-menu-icon">{p.icon}</span>}
      <span className="cmd-menu-label">{p.label}</span>
      {p.keys && (
        <span className="cmd-menu-keys" aria-hidden="true">
          {p.keys}
        </span>
      )}
    </span>
  );
}

// ---- icons: 16 px line drawings in the text color -----------------------------------

function Svg({ children, size = 16 }: { children: React.ReactNode; size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.3"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      {children}
    </svg>
  );
}

export const Icon = {
  newFolder: () => (
    <Svg>
      <path d="M1.8 12.6V3.6a1 1 0 0 1 1-1h3.1l1.4 1.6h5.9a1 1 0 0 1 1 1v2.2" />
      <path d="M1.8 12.6a1 1 0 0 0 1 1h6.4" />
      <path d="M12.3 9.4v4.6M10 11.7h4.6" />
    </Svg>
  ),
  cut: () => (
    <Svg>
      <circle cx="4.6" cy="11.6" r="2" />
      <circle cx="11.4" cy="11.6" r="2" />
      <path d="M6.1 10.3 12 2.4M9.9 10.3 4 2.4" />
    </Svg>
  ),
  copy: () => (
    <Svg>
      <rect x="5.3" y="5.3" width="8.4" height="8.6" rx="1.2" />
      <path d="M3.8 10.7h-.6a1 1 0 0 1-1-1V3.1a1 1 0 0 1 1-1h6.2a1 1 0 0 1 1 1v.6" />
    </Svg>
  ),
  paste: () => (
    <Svg>
      <path d="M5.6 2.9H4a1 1 0 0 0-1 1v9.2a1 1 0 0 0 1 1h8a1 1 0 0 0 1-1V3.9a1 1 0 0 0-1-1h-1.6" />
      <rect x="5.6" y="1.7" width="4.8" height="2.4" rx=".7" />
      <path d="M5.6 8h4.8M5.6 10.7h3.2" />
    </Svg>
  ),
  rename: () => (
    <Svg>
      <path d="M10.6 2.4l3 3-7.9 7.9H2.7v-3z" />
      <path d="M8.9 4.1l3 3M9.4 13.6h4.2" />
    </Svg>
  ),
  trash: () => (
    <Svg>
      <path d="M2.6 4.2h10.8M6.2 4.2V2.8h3.6v1.4" />
      <path d="M3.9 4.2l.7 9a1 1 0 0 0 1 .9h4.8a1 1 0 0 0 1-.9l.7-9" />
      <path d="M6.7 6.9v4.6M9.3 6.9v4.6" />
    </Svg>
  ),
  list: () => (
    <Svg>
      <path d="M2.4 3.6h1M5.6 3.6h8M2.4 8h1M5.6 8h8M2.4 12.4h1M5.6 12.4h8" />
    </Svg>
  ),
  tree: () => (
    <Svg>
      <rect x="1.8" y="2" width="4" height="3" rx=".6" />
      <rect x="9.6" y="6.5" width="4.6" height="3" rx=".6" />
      <rect x="9.6" y="11" width="4.6" height="3" rx=".6" />
      <path d="M3.8 5v7.5h5.8M3.8 8h5.8" />
    </Svg>
  ),
  grid: () => (
    <Svg>
      <rect x="2" y="2" width="5" height="5" rx="1" />
      <rect x="9" y="2" width="5" height="5" rx="1" />
      <rect x="2" y="9" width="5" height="5" rx="1" />
      <rect x="9" y="9" width="5" height="5" rx="1" />
    </Svg>
  ),
  map: () => (
    <Svg>
      <rect x="1.8" y="2" width="12.4" height="12" rx="1.2" />
      <path d="M8.6 2v12M8.6 8.6h5.6M1.8 10.4h6.8M11.4 8.6V14" />
    </Svg>
  ),
  details: () => (
    <Svg>
      <rect x="1.8" y="2.4" width="12.4" height="11.2" rx="1.2" />
      <path d="M9.6 2.4v11.2M11.2 5.4h1.4M11.2 7.6h1.4M11.2 9.8h1.4" />
    </Svg>
  ),
  diff: () => (
    <Svg>
      <path d="M4.2 13.4V5.6M2.2 7.6l2-2 2 2M11.8 2.6v7.8M9.8 8.4l2 2 2-2" />
    </Svg>
  ),
  /** A shield with "!": the judge's 위험도. */
  risk: () => (
    <Svg>
      <path d="M8 1.8l5 1.8v4.2c0 3-2.1 5.1-5 6.4-2.9-1.3-5-3.4-5-6.4V3.6z" />
      <path d="M8 5.2v3.4M8 10.9v.1" />
    </Svg>
  ),
  /** A broom: 정리 ▾. */
  tidy: () => (
    <Svg>
      <path d="M14 2 8.6 7.4" />
      <path d="M7.4 6.2l2.4 2.4" />
      <path d="M7.4 6.2 2 10.4 5.6 14l4.2-5.4" />
      <path d="M7.4 8.6 4.2 11.8" />
    </Svg>
  ),
  search: () => (
    <Svg>
      <circle cx="7" cy="7" r="4.4" />
      <path d="m10.3 10.3 3.6 3.6" />
    </Svg>
  ),
  more: () => (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor" aria-hidden="true" focusable="false">
      <circle cx="3.2" cy="8" r="1.3" />
      <circle cx="8" cy="8" r="1.3" />
      <circle cx="12.8" cy="8" r="1.3" />
    </svg>
  ),
  copyPath: () => (
    <Svg size={14}>
      <rect x="5.3" y="5.3" width="8.4" height="8.6" rx="1.2" />
      <path d="M3.8 10.7h-.6a1 1 0 0 1-1-1V3.1a1 1 0 0 1 1-1h6.2a1 1 0 0 1 1 1v.6" />
    </Svg>
  ),
};

/** The ▾ after a label: the button opens a menu. */
export function Caret() {
  return (
    <span className="cmd-caret" aria-hidden="true">
      ▾
    </span>
  );
}
