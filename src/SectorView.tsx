import { useState, useEffect, useCallback, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";

interface SectorData {
  bytes: number[];
  sector_size: number;
  user_data_offset: number;
  total_sectors: number;
  lba: number;
}

// Per-sector layout derived from the CD sync/header bytes.
interface Layout {
  hasCd: boolean;
  mode: number;
  form: number; // 0 = n/a or unknown, 1 = Form 1, 2 = Form 2 (Mode 2 only)
  syncEnd: number;
  headerEnd: number;
  subhdrEnd: number;
  dataStart: number;
  dataEnd: number;
  eccStart: number;
}

// Returns the ISOBuster-style mode label with user byte count, e.g. "Mode 2 / Form 1 (2048)".
function modeLabel(layout: Layout): string {
  const userBytes = layout.dataEnd - layout.dataStart;
  if (!layout.hasCd) return `Audio (${userBytes})`;
  if (layout.mode === 1) return `Mode 1 (${userBytes})`;
  if (layout.mode === 2) {
    if (layout.form === 1) return `Mode 2 / Form 1 (${userBytes})`;
    if (layout.form === 2) return `Mode 2 / Form 2 (${userBytes})`;
    return `Mode 2 (${userBytes})`;
  }
  return `Mode ${layout.mode} (${userBytes})`;
}

const CD_SYNC = [0x00,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0x00];

function getLayout(bytes: number[], sectorSize: number): Layout {
  const noLayout: Layout = {
    hasCd: false, mode: 0, form: 0,
    syncEnd: 0, headerEnd: 0, subhdrEnd: 0,
    dataStart: 0, dataEnd: sectorSize, eccStart: sectorSize,
  };
  if (sectorSize !== 2352 || bytes.length < 16) return noLayout;
  if (!CD_SYNC.every((v, i) => bytes[i] === v)) return noLayout;

  const mode = bytes[15];
  if (mode === 1) {
    // Mode 1: sync(12) + header(4) + data(2048) + EDC(4) + zero(8) + ECC(276) = 2352
    return { hasCd: true, mode, form: 0, syncEnd: 12, headerEnd: 16, subhdrEnd: 16, dataStart: 16, dataEnd: 2064, eccStart: 2064 };
  }
  if (mode === 2) {
    // Mode 2: sync(12) + header(4) + sub-header(8) + data + ECC/EDC
    // Form 2 flag: sub-header byte 2 (absolute byte 18), bit 5.
    const isForm2 = bytes.length >= 19 && (bytes[18] & 0x20) !== 0;
    const form = isForm2 ? 2 : 1;
    // Form 1: data(2048) + EDC(4) + ECC(276) ending at 2352; Form 2: data(2324) + EDC/spare(4) at 2348
    const dataEnd = isForm2 ? 2348 : 2072;
    return { hasCd: true, mode, form, syncEnd: 12, headerEnd: 16, subhdrEnd: 24, dataStart: 24, dataEnd, eccStart: dataEnd };
  }
  // Unknown mode but has CD sync
  return { hasCd: true, mode, form: 0, syncEnd: 12, headerEnd: 16, subhdrEnd: 16, dataStart: 16, dataEnd: 2352, eccStart: 2352 };
}

function bcd(b: number): number { return (b >> 4) * 10 + (b & 0x0F); }

function parseDisc(bytes: number[], sectorSize: number) {
  if (sectorSize !== 2352 || bytes.length < 16) return null;
  if (!CD_SYNC.every((v, i) => bytes[i] === v)) return null;
  const m = bcd(bytes[12]), s = bcd(bytes[13]), f = bcd(bytes[14]);
  const mode = bytes[15];
  const discLba = (m * 60 + s) * 75 + f - 150;
  const msf = `${String(m).padStart(2,'0')}:${String(s).padStart(2,'0')}:${String(f).padStart(2,'0')}`;
  return { mode, msf, discLba };
}

function byteClass(idx: number, layout: Layout): string {
  if (!layout.hasCd) return '';
  if (idx < layout.syncEnd) return 'hb-sync';
  if (idx < layout.headerEnd) return 'hb-hdr';
  if (idx < layout.subhdrEnd) return 'hb-sub';
  if (idx >= layout.dataStart && idx < layout.dataEnd) return '';
  return 'hb-ecc';
}

function HexRow({ offset, bytes, layout, diffMask }: {
  offset: number;
  bytes: number[];
  layout: Layout;
  diffMask?: boolean[];
}) {
  const content: (string | React.JSX.Element)[] = [];

  content.push(<span key="addr" className="hex-addr">{offset.toString(16).padStart(4, '0').toUpperCase()}</span>);
  content.push('  ');

  for (let j = 0; j < 16; j++) {
    if (j === 8) content.push('  '); else if (j > 0) content.push(' ');
    const cls = byteClass(offset + j, layout);
    const alt = j % 2 === 1 ? 'hb-alt' : '';
    const diff = diffMask?.[j] ? ' hb-diff' : '';
    const hex = bytes[j].toString(16).padStart(2, '0').toUpperCase();
    content.push(<span key={`h${j}`} className={`hb ${alt} ${cls}${diff}`}>{hex}</span>);
  }

  content.push('  |');

  for (let j = 0; j < 16; j++) {
    const b = bytes[j];
    const cls = byteClass(offset + j, layout);
    const alt = j % 2 === 1 ? 'hb-alt' : '';
    const diff = diffMask?.[j] ? ' hb-diff' : '';
    const ch = b >= 0x20 && b < 0x7F ? String.fromCharCode(b) : '.';
    content.push(<span key={`a${j}`} className={`ha ${alt} ${cls}${diff}`}>{ch}</span>);
  }

  content.push('|');

  return <div className="hex-row">{content}</div>;
}

function HexDump({ data, rawMode, compareBytes }: {
  data: SectorData;
  rawMode: boolean;
  compareBytes?: number[];
}) {
  const layout = getLayout(data.bytes, data.sector_size);
  const slice = (!rawMode && layout.hasCd)
    ? data.bytes.slice(layout.dataStart, layout.dataEnd)
    : data.bytes;
  const compareSlice = compareBytes
    ? ((!rawMode && layout.hasCd) ? compareBytes.slice(layout.dataStart, layout.dataEnd) : compareBytes)
    : undefined;

  const rows: React.JSX.Element[] = [];
  for (let i = 0; i < slice.length; i += 16) {
    const rowBytes = slice.slice(i, i + 16);
    const rowCmp = compareSlice?.slice(i, i + 16);
    const diffMask = rowCmp ? rowBytes.map((b, j) => rowCmp[j] !== undefined && b !== rowCmp[j]) : undefined;
    rows.push(
      <HexRow
        key={i}
        offset={i}
        bytes={rowBytes}
        layout={rawMode ? layout : { ...layout, hasCd: false }}
        diffMask={diffMask}
      />
    );
  }
  return <div className="sv-hex-dump">{rows}</div>;
}

export function SectorView({ imagePath, onClose, standalone, initialLba, initialCompareImagePath }: {
  imagePath: string;
  onClose: () => void;
  standalone?: boolean;
  initialLba?: number;
  initialCompareImagePath?: string | null;
}) {
  const [data, setData] = useState<SectorData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [inputVal, setInputVal] = useState("0");
  const [showExport, setShowExport] = useState(false);
  const [exportStart, setExportStart] = useState("0");
  const [exportEnd, setExportEnd] = useState("0");
  const [exporting, setExporting] = useState(false);
  const [exportStatus, setExportStatus] = useState<string | null>(null);
  // discOffset: constant difference between disc-absolute LBA (from sync header MSF)
  // and track-relative LBA. Null until we've seen a sector with a valid CD sync header.
  const [discOffset, setDiscOffset] = useState<number | null>(null);
  const [discMode, setDiscMode] = useState(false);
  const [rawMode, setRawMode] = useState(true);

  // Compare state
  const [showCompare, setShowCompare] = useState(!!initialCompareImagePath);
  const [compareImagePath, setCompareImagePath] = useState<string | null>(initialCompareImagePath ?? null);
  const [compareData, setCompareData] = useState<SectorData | null>(null);
  const [scanning, setScanning] = useState(false);
  const [noPrevDiff, setNoPrevDiff] = useState(false);
  const [noNextDiff, setNoNextDiff] = useState(false);
  const [allIdentical, setAllIdentical] = useState(false);

  const inputRef = useRef<HTMLInputElement>(null);

  const toDisplay = (trackLba: number) =>
    discMode && discOffset !== null ? trackLba + discOffset : trackLba;

  const toTrack = (displayLba: number) =>
    discMode && discOffset !== null ? displayLba - discOffset : displayLba;

  const load = useCallback(async (trackLba: number) => {
    setError(null);
    try {
      const result = await invoke<SectorData>("read_sector", { imagePath, lba: trackLba });
      const disc = parseDisc(result.bytes, result.sector_size);
      if (disc !== null) setDiscOffset(disc.discLba - result.lba);
      setData(result);
    } catch (e) {
      setError(String(e));
    }
  }, [imagePath]);

  // Load compare sector whenever the primary LBA or compare image changes.
  useEffect(() => {
    if (!compareImagePath || data === null) {
      setCompareData(null);
      return;
    }
    invoke<SectorData>("read_sector", { imagePath: compareImagePath, lba: data.lba })
      .then(setCompareData)
      .catch(() => setCompareData(null));
  }, [compareImagePath, data?.lba]);

  // Sync input field whenever data or mode changes.
  useEffect(() => {
    if (data) setInputVal(String(toDisplay(data.lba)));
  }, [data, discMode, discOffset]);

  useEffect(() => { load(initialLba ?? 0); }, [imagePath]);

  const lba = data?.lba ?? 0;
  const total = data?.total_sectors ?? 0;

  function go(targetTrackLba: number) {
    if (!total) return;
    load(Math.max(0, Math.min(targetTrackLba, total - 1)));
  }

  function commit() {
    const n = parseInt(inputVal, 10);
    if (!isNaN(n)) go(toTrack(n));
  }

  function toggleMode() {
    if (discOffset === null) return;
    setDiscMode(m => !m);
  }

  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.target instanceof HTMLInputElement) return;
      if (e.key === 'ArrowLeft'  || e.key === 'ArrowUp')   { e.preventDefault(); go(lba - 1); }
      if (e.key === 'ArrowRight' || e.key === 'ArrowDown')  { e.preventDefault(); go(lba + 1); }
      if (e.key === 'PageUp')   { e.preventDefault(); go(lba - 100); }
      if (e.key === 'PageDown') { e.preventDefault(); go(lba + 100); }
      if (e.key === 'Home')     { e.preventDefault(); go(0); }
      if (e.key === 'End')      { e.preventDefault(); go(total - 1); }
      if (e.key === 'Escape')   onClose();
    }
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [lba, total]);

  function openExport() {
    setExportStart(String(toDisplay(lba)));
    setExportEnd(String(toDisplay(lba)));
    setExportStatus(null);
    setShowExport(true);
  }

  async function runExport() {
    const start = parseInt(exportStart, 10);
    const end = parseInt(exportEnd, 10);
    if (isNaN(start) || isNaN(end) || end < start) {
      setExportStatus("Invalid range");
      return;
    }
    const destPath = await save({ defaultPath: "sectors.bin" });
    if (!destPath) return;
    setExporting(true);
    setExportStatus(null);
    try {
      const written = await invoke<number>("export_sector_range", {
        imagePath,
        lbaStart: toTrack(start),
        lbaEnd: toTrack(end),
        destPath,
      });
      setExportStatus(`Exported ${written} sector${written === 1 ? "" : "s"}`);
    } catch (e) {
      setExportStatus(String(e));
    } finally {
      setExporting(false);
    }
  }

  async function jumpToDiff(forward: boolean, fromLba = lba, inclusive = false) {
    if (!compareImagePath || scanning) return;
    setScanning(true);
    try {
      const found = await invoke<number | null>("find_diff_sector", {
        imagePathA: imagePath,
        imagePathB: compareImagePath,
        fromLba,
        forward,
        inclusive,
      });
      if (found !== null) {
        setNoPrevDiff(false);
        setNoNextDiff(false);
        setAllIdentical(false);
        go(found);
      } else {
        if (forward) {
          setNoNextDiff(true);
          if (noPrevDiff || lba === 0) setAllIdentical(true);
        } else {
          setNoPrevDiff(true);
          if (noNextDiff || lba >= total - 1) setAllIdentical(true);
        }
      }
    } finally {
      setScanning(false);
    }
  }

  async function pickCompareImage() {
    const path = await open({
      title: "Open disc image to compare",
      filters: [{ name: "Disc Images", extensions: ["iso","img","chd","cue","mds","mdx","nrg","ccd","cdi","gdi","toc","b5t","b6t","bwt","c2d","pdi","gi","daa","cso","ciso","ecm","wbfs","wux","wud","sdram","sbram","aif","cif","uif","skeleton","skeleton.zst","iso.zst","img.zst"] }],
    });
    if (path && typeof path === "string") {
      setCompareImagePath(path);
      setCompareData(null);
      setNoPrevDiff(false);
      setNoNextDiff(false);
      setAllIdentical(false);
    }
  }

  function clearCompare() {
    setCompareImagePath(null);
    setCompareData(null);
    setNoPrevDiff(false);
    setNoNextDiff(false);
    setAllIdentical(false);
  }

  const disc = data ? parseDisc(data.bytes, data.sector_size) : null;
  const layout = data ? getLayout(data.bytes, data.sector_size) : null;
  const isLastSector = total > 0 && lba >= total - 1;

  const minInput = discMode && discOffset !== null ? discOffset : 0;
  const maxInput = discMode && discOffset !== null ? total - 1 + discOffset : total > 0 ? total - 1 : 0;

  const compareFileName = compareImagePath?.split(/[/\\]/).pop() ?? null;

  const inner = (
    <div className={`${standalone ? "sv-standalone" : `modal sv-modal${compareData ? " sv-modal--compare" : ""}`}`} onClick={e => e.stopPropagation()}>

      <div className="modal-header">
        <span />
        <span className="modal-title">Sector View</span>
        <div className="sv-header-btns">
          {!standalone && (
            <button
              className="sv-detach"
              title="Open in separate window"
              onClick={async () => {
                await invoke("open_sector_view_window", { imagePath, lba: data?.lba ?? 0, compareImagePath: compareImagePath ?? null });
                onClose();
              }}
            >⧉</button>
          )}
          <button className="modal-close" onClick={onClose}>✕</button>
        </div>
      </div>

        <div className="sv-nav">
          <button
            className={`sv-mode-toggle ${discMode ? 'sv-mode-toggle--active' : ''}`}
            onClick={toggleMode}
            disabled={discOffset === null}
            title={discOffset === null ? 'No CD sync header — disc LBA unavailable' : discMode ? 'Switch to track-relative LBA' : 'Switch to disc-absolute LBA'}
          >
            {discMode ? 'Disc LBA' : 'Track LBA'}
          </button>
          <input
            ref={inputRef}
            className="sv-input"
            type="number"
            min={minInput}
            max={maxInput}
            value={inputVal}
            onChange={e => setInputVal(e.target.value)}
            onKeyDown={e => { if (e.key === 'Enter') commit(); }}
            onBlur={commit}
          />
          {total > 0 && (
            <span className="sv-total">
              of {(discMode && discOffset !== null ? total - 1 + discOffset : total - 1).toLocaleString()}
            </span>
          )}
          <div className="sv-nav-btns">
            <button className="sv-btn" onClick={() => go(0)}           disabled={lba === 0}     title="First (Home)">⏮</button>
            <button className="sv-btn" onClick={() => go(lba - 1)}     disabled={lba === 0}     title="Previous (←)">◀</button>
            <button className="sv-btn" onClick={() => go(lba + 1)}     disabled={isLastSector}  title="Next (→)">▶</button>
            <button className="sv-btn" onClick={() => go(total - 1)}   disabled={isLastSector}  title="Last (End)">⏭</button>
          </div>
          {layout?.hasCd && (
            <button
              className={`sv-mode-toggle ${!rawMode ? 'sv-mode-toggle--active' : ''}`}
              onClick={() => setRawMode(m => !m)}
              title={rawMode
                ? `Show user data only (${layout.dataEnd - layout.dataStart}B)`
                : `Show full raw sector (${data!.sector_size}B)`}
            >
              {rawMode ? `${data!.sector_size}B raw` : `${layout.dataEnd - layout.dataStart}B user`}
            </button>
          )}
          <button
            className={`sv-mode-toggle ${showExport ? 'sv-mode-toggle--active' : ''}`}
            onClick={() => showExport ? setShowExport(false) : openExport()}
            disabled={!data}
            title="Export sector range as raw binary"
          >Export range</button>
          <button
            className={`sv-mode-toggle ${showCompare ? 'sv-mode-toggle--active' : ''}`}
            onClick={() => { setShowCompare(s => !s); }}
            disabled={!data}
            title="Compare with a second disc image"
          >Compare</button>
        </div>

        {showExport && (
          <div className="sv-export-row">
            <span className="sv-export-label">LBA</span>
            <input
              className="sv-input"
              type="number"
              min={minInput}
              max={maxInput}
              value={exportStart}
              onChange={e => setExportStart(e.target.value)}
            />
            <span className="sv-export-label">to</span>
            <input
              className="sv-input"
              type="number"
              min={minInput}
              max={maxInput}
              value={exportEnd}
              onChange={e => setExportEnd(e.target.value)}
            />
            <button className="sv-btn sv-export-btn" onClick={runExport} disabled={exporting}>
              {exporting ? "Exporting…" : "Export .bin"}
            </button>
            {exportStatus && <span className="sv-export-status">{exportStatus}</span>}
          </div>
        )}

        {showCompare && (
          <div className="sv-compare-row">
            <button className="sv-btn sv-export-btn" onClick={pickCompareImage}>
              {compareImagePath ? "Change…" : "Open image…"}
            </button>
            {compareFileName
              ? <span className="sv-compare-path" title={compareImagePath ?? ""}>{compareFileName}</span>
              : <span className="sv-export-label">No image selected</span>
            }
            {compareImagePath && (
              <button className="sv-btn sv-export-btn" onClick={clearCompare}>Clear</button>
            )}
            {compareData && (
              <>
                <span className="sv-compare-diff-count">
                  {allIdentical
                    ? <span style={{ color: "#4ec94e" }}>Images identical</span>
                    : (() => {
                        const a = data!.bytes;
                        const b = compareData.bytes;
                        const len = Math.max(a.length, b.length);
                        let diff = 0;
                        for (let i = 0; i < len; i++) if (a[i] !== b[i]) diff++;
                        return diff === 0
                          ? <span style={{ color: "#4ec94e" }}>Sector identical</span>
                          : <span style={{ color: "#e5a550" }}>{diff.toLocaleString()} byte{diff !== 1 ? "s" : ""} differ</span>;
                      })()
                  }
                </span>
                <button className="sv-btn sv-export-btn" onClick={() => jumpToDiff(false)} disabled={scanning || lba === 0 || noPrevDiff} title={noPrevDiff ? "No previous differences found" : "Jump to previous differing sector"}>
                  ◀ Prev diff
                </button>
                <button className="sv-btn sv-export-btn" onClick={() => jumpToDiff(true)} disabled={scanning || lba >= total - 1 || noNextDiff} title={noNextDiff ? "No further differences found" : "Jump to next differing sector"}>
                  {scanning ? "Scanning…" : "Next diff ▶"}
                </button>
              </>
            )}
          </div>
        )}

        {data && (
          <div className="sv-info">
            <div className="sv-info-left">
              {disc ? (
                <>
                  <span className="sv-badge sv-badge-cd">CD</span>
                  <span>{layout ? modeLabel(layout) : `Mode ${disc.mode}`}</span>
                  <span className="sv-sep">·</span>
                  {discMode
                    ? <span title={`Track-relative LBA ${data.lba}`}>Disc LBA <strong>{disc.discLba.toLocaleString()}</strong></span>
                    : <span title={`Disc-absolute LBA ${disc.discLba}`}>Track LBA <strong>{data.lba.toLocaleString()}</strong></span>
                  }
                  <span className="sv-sep">·</span>
                  <span>MSF {disc.msf}</span>
                </>
              ) : (
                <>
                  <span className="sv-badge sv-badge-iso">ISO</span>
                  <span>{data.sector_size}B / sector</span>
                </>
              )}
            </div>
            <div className="sv-legend">
              {layout?.hasCd && <><span className="sv-leg"><span className="sv-swatch hb-sync"/>Sync</span><span className="sv-leg"><span className="sv-swatch hb-hdr"/>Hdr</span></>}
              {layout?.hasCd && layout.subhdrEnd > layout.headerEnd && <span className="sv-leg"><span className="sv-swatch hb-sub"/>Sub-Hdr</span>}
              {layout?.hasCd && <span className="sv-leg"><span className="sv-swatch hb-ecc"/>ECC</span>}
              {compareData && <span className="sv-leg"><span className="sv-swatch hb-diff"/>Diff</span>}
            </div>
          </div>
        )}

        {error && <div className="sv-error">{error}</div>}

        <div className={`sv-hex-area ${compareData ? "sv-hex-area--compare" : ""}`}>
          {data && (
            <div className={compareData ? "sv-hex-panel" : undefined}>
              {compareData && <div className="sv-hex-panel-label">Image A — {imagePath.split(/[/\\]/).pop()}</div>}
              <HexDump data={data} rawMode={rawMode} compareBytes={compareData?.bytes} />
            </div>
          )}
          {compareData && (
            <div className="sv-hex-panel">
              <div className="sv-hex-panel-label">Image B — {compareFileName}</div>
              <HexDump data={compareData} rawMode={rawMode} compareBytes={data?.bytes} />
            </div>
          )}
        </div>

      </div>
  );

  if (standalone) return inner;
  return <div className="modal-overlay" onClick={onClose}>{inner}</div>;
}
