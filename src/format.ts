const UNITS = ["B", "KB", "MB", "GB", "TB"];

export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  let v = bytes;
  let u = 0;
  while (v >= 1024 && u < UNITS.length - 1) {
    v /= 1024;
    u++;
  }
  return `${v >= 100 ? v.toFixed(0) : v.toFixed(1)} ${UNITS[u]}`;
}

export function filetimeToDate(ft: number): Date | null {
  if (!ft) return null;
  return new Date(ft / 10000 - 11644473600000);
}

export function formatDate(ft: number): string {
  const d = filetimeToDate(ft);
  if (!d || d.getFullYear() < 1980) return "";
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

export function timeAgo(ft: number): string {
  const d = filetimeToDate(ft);
  if (!d) return "";
  const s = (Date.now() - d.getTime()) / 1000;
  if (s < 60) return "방금";
  if (s < 3600) return `${Math.floor(s / 60)}분 전`;
  if (s < 86400) return `${Math.floor(s / 3600)}시간 전`;
  return `${Math.floor(s / 86400)}일 전`;
}

const TYPES: Record<string, string> = {
  exe: "응용 프로그램", msi: "설치 패키지", dll: "라이브러리", sys: "시스템 파일",
  zip: "압축 파일", "7z": "압축 파일", rar: "압축 파일",
  jpg: "이미지", jpeg: "이미지", png: "이미지", gif: "이미지", webp: "이미지", bmp: "이미지",
  mp4: "동영상", mkv: "동영상", avi: "동영상", mov: "동영상", webm: "동영상",
  mp3: "오디오", wav: "오디오", flac: "오디오",
  pdf: "PDF 문서", txt: "텍스트", md: "Markdown", doc: "Word 문서", docx: "Word 문서",
  xls: "Excel 문서", xlsx: "Excel 문서", ppt: "PowerPoint", pptx: "PowerPoint", hwp: "한글 문서", hwpx: "한글 문서",
  iso: "디스크 이미지", vhdx: "가상 디스크", log: "로그",
};

export function typeLabel(name: string, isDir: boolean): string {
  if (isDir) return "폴더";
  const dot = name.lastIndexOf(".");
  if (dot <= 0) return "파일";
  const ext = name.slice(dot + 1).toLowerCase();
  return TYPES[ext] ?? `${ext.toUpperCase()} 파일`;
}

export function parentOf(path: string): string | null {
  const trimmed = path.replace(/[\\/]+$/, "");
  const i = trimmed.lastIndexOf("\\");
  if (i < 0) return null;
  if (i <= 2) return trimmed.length > 3 ? trimmed.slice(0, i + 1) : null;
  return trimmed.slice(0, i);
}

/** Cleans a typed path. Returns "" for an empty one, so callers can ignore it. */
export function normalizePath(p: string): string {
  let s = p.trim().replace(/\//g, "\\");
  // Collapse doubled separators (C:\\x, C://x), keeping the leading \\ of a UNC path.
  s = (s.startsWith("\\\\") ? "\\" : "") + s.replace(/\\{2,}/g, "\\");
  s = s.replace(/^[a-z]:/, (d) => d.toUpperCase());
  // A bare drive is its root: "C:" alone means the current directory on C:.
  if (/^[A-Z]:\\?$/.test(s)) return `${s.slice(0, 2)}\\`;
  return s.replace(/\\+$/, "");
}

/** A path in one comparable form: cleaned, lower case, no trailing separator ("C:\" is "c:"). */
export function pathKey(p: string): string {
  return normalizePath(p).toLowerCase().replace(/\\$/, "");
}

/** Whether `p` is `folder` or somewhere inside it. Both are pathKey()s. */
export function isSameOrInside(p: string, folder: string): boolean {
  return p === folder || p.startsWith(`${folder}\\`);
}

/** "C:" from a drive name or path, in the case DriveInfo.letter uses. */
export function driveKey(p: string): string {
  return p.slice(0, 2).toUpperCase();
}

/** A tab's name: the folder's own name, or the drive ("C:") for a drive root. */
export function folderTitle(path: string): string {
  const t = path.replace(/\\+$/, "");
  if (/^[A-Za-z]:$/.test(t)) return t.toUpperCase();
  return t.slice(t.lastIndexOf("\\") + 1) || path;
}
