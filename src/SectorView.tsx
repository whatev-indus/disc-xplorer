import { useState, useEffect, useCallback, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";

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
  syncEnd: number;
  headerEnd: number;
  subhdrEnd: number;
  dataStart: number;
  dataEnd: number;
  eccStart: number;
}

const CD_SYNC = [0x00,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0x00];

function getLayout(bytes: number[], sectorSize: number): Layout {
  const noLayout: Layout = {
    hasCd: false, mode: 0,
    syncEnd: 0, headerEnd: 0, subhdrEnd: 0,
    dataStart: 0, dataEnd: sectorSize, eccStart: sectorSize,
  };
  if (sectorSize !== 2352 || bytes.length < 16) return noLayout;
  if (!CD_SYNC.every((v, i) => bytes[i] === v)) return noLayout;

  const mode = bytes[15];
  if (mode === 1) {
    return { hasCd: true, mode, syncEnd: 12, headerEnd: 16, subhdrEnd: 16, dataStart: 16, dataEnd: 2064, eccStart: 2064 };
  }
  if (mode === 2) {
    const isForm2 = bytes.length >= 19 && (bytes[18] & 0x20) !== 0;
    const dataEnd = isForm2 ? 2348 : 2072;
    return { hasCd: true, mode, syncEnd: 12, headerEnd: 16, subhdrEnd: 24, dataStart: 24, dataEnd, eccStart: dataEnd };
  }
  // Unknown mode but has sync
  return { hasCd: true, mode, syncEnd: 12, headerEnd: 16, subhdrEnd: 16, dataStart: 16, dataEnd: 2352, eccStart: 2352 };
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

function HexRow({ offset, bytes, layout }: { offset: number; bytes: number[]; layout: Layout }) {
  // Build row as array of strings and elements — parent uses white-space:pre
  const content: (string | React.JSX.Element)[] = [];

  content.push(<span key="addr" className="hex-addr">{offset.toString(16).padStart(4, '0').toUpperCase()}</span>);
  content.push('  ');

  for (let j = 0; j < 16; j++) {
    if (j === 8) content.push('  '); else if (j > 0) content.push(' ');
    const cls = byteClass(offset + j, layout);
    const hex = bytes[j].toString(16).padStart(2, '0').toUpperCase();
    content.push(<span key={`h${j}`} className={`hb ${cls}`}>{hex}</span>);
  }

  content.push('  |');

  for (let j = 0; j < 16; j++) {
    const b = bytes[j];
    const cls = byteClass(offset + j, layout);
    const ch = b >= 0x20 && b < 0x7F ? String.fromCharCode(b) : '.';
    content.push(<span key={`a${j}`} className={`ha ${cls}`}>{ch}</span>);
  }

  content.push('|');

  return <div className="hex-row">{content}</div>;
}

function HexDump({ data }: { data: SectorData }) {
  const layout = getLayout(data.bytes, data.sector_size);
  const rows: React.JSX.Element[] = [];
  for (let i = 0; i < data.bytes.length; i += 16) {
    rows.push(<HexRow key={i} offset={i} bytes={data.bytes.slice(i, i + 16)} layout={layout} />);
  }
  return <div className="sv-hex-dump">{rows}</div>;
}

export function SectorView({ imagePath, onClose }: { imagePath: string; onClose: () => void }) {
  const [data, setData] = useState<SectorData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [inputVal, setInputVal] = useState("0");
  const inputRef = useRef<HTMLInputElement>(null);

  const load = useCallback(async (lba: number) => {
    setError(null);
    try {
      const result = await invoke<SectorData>("read_sector", { imagePath, lba });
      setData(result);
      setInputVal(String(lba));
    } catch (e) {
      setError(String(e));
    }
  }, [imagePath]);

  useEffect(() => { load(0); }, [imagePath]);

  const lba = data?.lba ?? 0;
  const total = data?.total_sectors ?? 0;

  function go(target: number) {
    if (!total) return;
    load(Math.max(0, Math.min(target, total - 1)));
  }

  function commit() {
    const n = parseInt(inputVal, 10);
    if (!isNaN(n)) go(n);
  }

  // Keyboard navigation (when input isn't focused)
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

  const disc = data ? parseDisc(data.bytes, data.sector_size) : null;
  const layout = data ? getLayout(data.bytes, data.sector_size) : null;
  const isLastSector = total > 0 && lba >= total - 1;

  return (
    <div className="modal-overlay" onClick={onClose}>
      <div className="modal sv-modal" onClick={e => e.stopPropagation()}>

        <div className="modal-header">
          <span className="modal-title">Sector View</span>
          <button className="modal-close" onClick={onClose}>✕</button>
        </div>

        <div className="sv-nav">
          <span className="sv-nav-label">Sector</span>
          <input
            ref={inputRef}
            className="sv-input"
            type="number"
            min={0}
            max={total > 0 ? total - 1 : 0}
            value={inputVal}
            onChange={e => setInputVal(e.target.value)}
            onKeyDown={e => { if (e.key === 'Enter') commit(); }}
            onBlur={commit}
          />
          {total > 0 && <span className="sv-total">of {(total - 1).toLocaleString()}</span>}
          <div className="sv-nav-btns">
            <button className="sv-btn" onClick={() => go(0)}           disabled={lba === 0}     title="First (Home)">⏮</button>
            <button className="sv-btn" onClick={() => go(lba - 1)}     disabled={lba === 0}     title="Previous (←)">◀</button>
            <button className="sv-btn" onClick={() => go(lba + 1)}     disabled={isLastSector}  title="Next (→)">▶</button>
            <button className="sv-btn" onClick={() => go(total - 1)}   disabled={isLastSector}  title="Last (End)">⏭</button>
          </div>
        </div>

        {data && (
          <div className="sv-info">
            <div className="sv-info-left">
              {disc ? (
                <>
                  <span className="sv-badge sv-badge-cd">CD</span>
                  <span>Mode {disc.mode}</span>
                  <span className="sv-sep">·</span>
                  <span>LBA <strong>{data.lba.toLocaleString()}</strong></span>
                  <span className="sv-sep">·</span>
                  <span title={`Disc-absolute LBA ${disc.discLba}`}>MSF {disc.msf}</span>
                  <span className="sv-sep">·</span>
                  <span>{data.sector_size}B raw</span>
                  {layout && layout.dataEnd > layout.dataStart && (
                    <><span className="sv-sep">·</span><span>{layout.dataEnd - layout.dataStart}B user</span></>
                  )}
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
            </div>
          </div>
        )}

        {error && <div className="sv-error">{error}</div>}

        <div className="sv-hex-area">
          {data && <HexDump data={data} />}
        </div>

      </div>
    </div>
  );
}
