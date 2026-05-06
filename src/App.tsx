import { useState, useCallback, useRef, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { getCurrentWindow } from "@tauri-apps/api/window";

const IS_SECTOR_VIEW_WINDOW = getCurrentWindow().label.startsWith("sv");
import { open, save } from "@tauri-apps/plugin-dialog";
import { SectorView } from "./SectorView";
import "./App.css";

interface DiscEntry {
  name: string;
  is_dir: boolean;
  lba: number;
  size: number;
  size_bytes: number;
  modified: string;
}

interface AudioEntry {
  track_number: number;
  name: string;
  start_lba: number;
  num_sectors: number;
  size_bytes: number;
  format: string;
  is_data: boolean;
}

interface DriveInfo {
  name: string;
  device_path: string;
  has_disc: boolean;
  volume_name: string | null;
  mount_point: string | null;
}

type NodeType = "root" | "session" | "data_track" | "audio_track" | "filesystem" | "dir";
type ViewMode = "filesystem" | "audio" | "empty-drive";

interface TreeNode {
  name: string;
  path: string;
  nodeType: NodeType;
  children: TreeNode[] | null;
  expanded: boolean;
}

interface TrackInfo {
  number: number;
  is_data: boolean;
  mode: string;
  start_lba: number;
  num_sectors: number;
  session: number;
  bin_path: string;
}

interface MountResult {
  mount_point: string;
  device: string;
}

interface EmulatedDrive {
  slot: string;
  device: string;
  image_path: string;
}

interface ColWidths {
  name: number;
  lba: number;
  size: number;
  modified: number;
  save: number;
}

function formatSize(bytes: number): string {
  if (bytes === 0) return "—";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function formatDuration(sectors: number): string {
  if (sectors === 0) return "—";
  const totalSeconds = Math.floor(sectors / 75);
  const m = Math.floor(totalSeconds / 60);
  const s = totalSeconds % 60;
  const f = sectors % 75;
  return `${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}.${String(f).padStart(2, "0")}`;
}

function isMountable(path: string, platform: string): boolean {
  const lower = path.toLowerCase();
  if (lower.endsWith(".iso") || lower.endsWith(".img")) return true;
  if (platform === "macos" && (lower.endsWith(".dmg") || lower.endsWith(".cdr"))) return true;
  if (platform === "linux" && (
    lower.endsWith(".cue") || lower.endsWith(".mds") || lower.endsWith(".mdx") ||
    lower.endsWith(".nrg") || lower.endsWith(".ccd") ||
    lower.endsWith(".toc") || lower.endsWith(".b6t") || lower.endsWith(".bwt") ||
    lower.endsWith(".c2d") || lower.endsWith(".pdi") || lower.endsWith(".gi") ||
    lower.endsWith(".daa")
  )) return true;
  return false;
}

function TreeItem({
  node, imagePath, selectedPath, onSelect, onToggle, depth,
}: {
  node: TreeNode; imagePath: string; selectedPath: string;
  onSelect: (path: string) => void; onToggle: (path: string) => void; depth: number;
}) {
  const isAudio = node.nodeType === "audio_track";
  const isSession = node.nodeType === "session";
  const isDataTrack = node.nodeType === "data_track";
  const isFilesystem = node.nodeType === "filesystem";

  const icon = isSession || isDataTrack ? "📀"
    : isAudio ? "🎵"
    : isFilesystem ? "🗂️"
    : node.nodeType === "dir" ? "📁"
    : "💿";

  const alwaysExpanded = isSession;
  const noArrow = isAudio || isFilesystem || alwaysExpanded;

  function handleClick() {
    onSelect(node.path);
    if (!isAudio && !isFilesystem && !isSession) onToggle(node.path);
  }

  return (
    <div>
      <div
        className={[
          "tree-item",
          node.path === selectedPath ? "tree-item--selected" : "",
          isAudio ? "tree-item--audio" : "",
          isSession ? "tree-item--session" : "",
          isFilesystem ? "tree-item--filesystem" : "",
        ].filter(Boolean).join(" ")}
        style={{ paddingLeft: `${depth * 16 + 8}px` }}
        onClick={handleClick}
      >
        <span className="tree-arrow">
          {noArrow ? " " : (node.children === null ? " " : node.expanded ? "▾" : "▶")}
        </span>
        <span className="tree-icon">{icon}</span>
        <span className="tree-label">{node.name}</span>
      </div>
      {(alwaysExpanded || node.expanded) && node.children && (
        <div>
          {node.children.map((child) => (
            <TreeItem key={child.path} node={child} imagePath={imagePath}
              selectedPath={selectedPath} onSelect={onSelect} onToggle={onToggle}
              depth={depth + 1} />
          ))}
        </div>
      )}
    </div>
  );
}

function App() {
  const [imagePath, setImagePath] = useState<string | null>(null);
  const [sourceImagePath, setSourceImagePath] = useState<string | null>(null);
  const [imageName, setImageName] = useState<string>("");
  const [currentPath, setCurrentPath] = useState("/");
  const [entries, setEntries] = useState<DiscEntry[]>([]);
  const [audioEntries, setAudioEntries] = useState<AudioEntry[]>([]);
  const [viewMode, setViewMode] = useState<ViewMode>("filesystem");
  const [emptyDriveName, setEmptyDriveName] = useState<string | null>(null);
  const [tree, setTree] = useState<TreeNode[]>([]);
  const [cueTracks, setCueTracks] = useState<TrackInfo[]>([]);
  const [activeFilesystem, setActiveFilesystem] = useState<string>("");
  const [sidebarPath, setSidebarPath] = useState<string>("");
  const [error, setError] = useState<string | null>(null);
  const [statusText, setStatusText] = useState("No disc loaded");
  const [mountedDevice, setMountedDevice] = useState<string | null>(null);
  const [physicalDiscActive, setPhysicalDiscActive] = useState(false);
  const [drives, setDrives] = useState<DriveInfo[]>([]);
  const [showDriveMenu, setShowDriveMenu] = useState(false);
  const [loadingDrives, setLoadingDrives] = useState(false);
  const [colWidths, setColWidths] = useState<ColWidths>({
    name: 280, lba: 80, size: 110, modified: 160, save: 56,
  });
  const [showSettings, setShowSettings] = useState(false);
  const [showLicenses, setShowLicenses] = useState(false);
  const [audioFormat, setAudioFormat] = useState<"wav" | "flac" | "mp3">("wav");
  const [defaultDownloadPath, setDefaultDownloadPath] = useState<string>("");
  const [isDragOver, setIsDragOver] = useState(false);
  const [showSectorView, setShowSectorView] = useState(false);
  const [platform, setPlatform] = useState<string>("");
  const [showCdemuPrompt, setShowCdemuPrompt] = useState(false);
  const [cdemuInstalling, setCdemuInstalling] = useState(false);
  const [cdemuInstallMsg, setCdemuInstallMsg] = useState<string | null>(null);
  const [cdemuInstallOk, setCdemuInstallOk] = useState(false);
  const [emulatedDrives, setEmulatedDrives] = useState<EmulatedDrive[]>([]);
  const [emulating, setEmulating] = useState(false);
  const [svParams, setSvParams] = useState<{ imagePath: string; lba: number } | null>(null);
  const [updateVersion, setUpdateVersion] = useState<string | null>(null);

  useEffect(() => {
    if (!IS_SECTOR_VIEW_WINDOW) return;
    invoke<{ image_path: string; lba: number } | null>("claim_sector_view_params").then(p => {
      if (p) setSvParams({ imagePath: p.image_path, lba: p.lba });
    });
  }, []);

  const dragRef = useRef<{ col: keyof ColWidths; startX: number; startWidth: number } | null>(null);
  const driveMenuRef = useRef<HTMLDivElement>(null);
  const settingsRef = useRef<HTMLDivElement>(null);
  const settingsGearRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    invoke<string>("get_platform").then(setPlatform);
  }, []);

  async function checkForUpdate() {
    if (IS_SECTOR_VIEW_WINDOW) return;
    try {
      const v = await invoke<string | null>("check_for_update");
      if (v) setUpdateVersion(v);
    } catch { /* ignore */ }
  }

  useEffect(() => {
    if (platform !== "linux") return;
    invoke<boolean>("check_cdemu_installed").then(installed => {
      if (!installed) setShowCdemuPrompt(true);
    });
  }, [platform]);

  useEffect(() => {
    function handleOutsideClick(e: MouseEvent) {
      if (driveMenuRef.current && !driveMenuRef.current.contains(e.target as Node)) {
        setShowDriveMenu(false);
      }
    }
    if (showDriveMenu) document.addEventListener("mousedown", handleOutsideClick);
    return () => document.removeEventListener("mousedown", handleOutsideClick);
  }, [showDriveMenu]);

  useEffect(() => {
    function handleOutsideClick(e: MouseEvent) {
      if (
        settingsRef.current && !settingsRef.current.contains(e.target as Node) &&
        settingsGearRef.current && !settingsGearRef.current.contains(e.target as Node)
      ) {
        setShowSettings(false);
      }
    }
    if (showSettings) document.addEventListener("mousedown", handleOutsideClick);
    return () => document.removeEventListener("mousedown", handleOutsideClick);
  }, [showSettings]);

  async function installCdemu() {
    setCdemuInstalling(true);
    setCdemuInstallMsg(null);
    try {
      const msg = await invoke<string>("install_cdemu");
      setCdemuInstallMsg(msg);
      setCdemuInstallOk(true);
    } catch (e) {
      setCdemuInstallMsg(String(e));
      setCdemuInstallOk(false);
    } finally {
      setCdemuInstalling(false);
    }
  }

  async function pickDownloadLocation() {
    const dir = await open({ directory: true, title: "Set Default Download Location" });
    if (dir) setDefaultDownloadPath(dir as string);
  }

  function onResizeStart(col: keyof ColWidths, e: React.MouseEvent) {
    e.preventDefault();
    dragRef.current = { col, startX: e.clientX, startWidth: colWidths[col] };
    function onMove(e: MouseEvent) {
      if (!dragRef.current) return;
      const delta = e.clientX - dragRef.current.startX;
      setColWidths((prev) => ({
        ...prev,
        [dragRef.current!.col]: Math.max(48, dragRef.current!.startWidth + delta),
      }));
    }
    function onUp() {
      dragRef.current = null;
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    }
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  }

  const navIdRef = useRef(0);

  const loadDirectory = useCallback(async (imgPath: string, dirPath: string, fsLabel = "", filesystem?: string) => {
    const myId = ++navIdRef.current;
    setError(null);
    const effectiveFs = filesystem !== undefined ? filesystem : activeFilesystem;
    if (filesystem !== undefined) setActiveFilesystem(filesystem);
    try {
      const result = await invoke<DiscEntry[]>("list_disc_contents", {
        imagePath: imgPath,
        dirPath,
        filesystem: effectiveFs || null,
      });
      if (navIdRef.current !== myId) return;
      const sorted = result.sort((a, b) => {
        if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1;
        return a.name.localeCompare(b.name);
      });
      setEntries(sorted);
      setAudioEntries([]);
      setViewMode("filesystem");
      setCurrentPath(dirPath);
      const dirs = sorted.filter((e) => e.is_dir).length;
      const files = sorted.filter((e) => !e.is_dir).length;
      setStatusText(`${dirs} folder${dirs !== 1 ? "s" : ""}, ${files} file${files !== 1 ? "s" : ""}${fsLabel ? ` · ${fsLabel}` : ""}`);
    } catch (e) {
      if (navIdRef.current !== myId) return;
      setError(String(e));
    }
  }, [activeFilesystem]);

  function buildAudioEntries(tracks: TrackInfo[]): AudioEntry[] {
    return tracks.map((t) => ({
      track_number: t.number,
      name: `Track ${String(t.number).padStart(2, "0")}`,
      start_lba: t.start_lba,
      num_sectors: t.num_sectors,
      size_bytes: t.is_data ? t.num_sectors * 2048 : t.num_sectors * 2352,
      format: t.is_data ? t.mode : "CD Audio",
      is_data: t.is_data,
    }));
  }

  async function openImageAtPath(path: string) {
    const name = path.split(/[/\\]/).pop() ?? path;
    setActiveFilesystem("");
    setImagePath(path);
    setSourceImagePath(path);
    setImageName(name);
    setError(null);
    setEmptyDriveName(null);
    setMountedDevice(null);
    setPhysicalDiscActive(false);

    const lowerPath = path.toLowerCase();
    const isCue = lowerPath.endsWith(".cue");
    const isMds = lowerPath.endsWith(".mds");
    const isGdi = lowerPath.endsWith(".gdi");
    const isCdi = lowerPath.endsWith(".cdi");

    if (isCue || isMds || isGdi || isCdi) {
      const [tracks, filesystems] = await Promise.all([
        isGdi
          ? invoke<TrackInfo[]>("get_gdi_tracks", { gdiPath: path })
          : isMds
            ? invoke<TrackInfo[]>("get_mds_tracks", { mdsPath: path })
            : isCdi
              ? invoke<TrackInfo[]>("get_cdi_tracks", { cdiPath: path })
              : invoke<TrackInfo[]>("get_cue_tracks", { cuePath: path }),
        invoke<string[]>("get_disc_filesystems", { imagePath: path }).catch(() => ["ISO 9660"]),
      ]);
      setCueTracks(tracks);
      setSidebarPath("__root");

      const sessions = [...new Set(tracks.map((t) => t.session))].sort((a, b) => a - b);
      const multiSession = sessions.length > 1;

      const makeFsChildren = (): TreeNode[] =>
        filesystems.map((fs) => ({
          name: fs,
          path: `__fs_${fs.toLowerCase().replace(/ /g, "_")}`,
          nodeType: "filesystem" as NodeType,
          children: null,
          expanded: false,
        }));

      const makeTrackNode = (t: TrackInfo): TreeNode => t.is_data
        ? {
            name: t.mode === "CDI/PREGAP"
              ? `Track ${String(t.number).padStart(2, "0")} Pregap — CD-i`
              : `Track ${String(t.number).padStart(2, "0")} — ${t.mode}`,
            path: `__track_${t.number}`,
            nodeType: "data_track",
            children: makeFsChildren(),
            expanded: true,
          }
        : {
            name: `Track ${String(t.number).padStart(2, "0")} — ${t.mode}`,
            path: `__audio_${t.number}`,
            nodeType: "audio_track",
            children: null,
            expanded: false,
          };

      const trackNodes: TreeNode[] = multiSession
        ? sessions.map((s): TreeNode => {
            const sessionTracks = tracks.filter((t) => t.session === s);
            const hasData = sessionTracks.some((t) => t.is_data);
            return {
              name: `Session ${s} — ${hasData ? "Data" : "Audio"}`,
              path: `__session_${s}`,
              nodeType: "session",
              children: sessionTracks.map(makeTrackNode),
              expanded: true,
            };
          })
        : tracks.map(makeTrackNode);

      const rootNode: TreeNode = {
        name, path: "__root", nodeType: "root", children: trackNodes, expanded: true,
      };
      setTree([rootNode]);

      // Show audio tracks on initial open; user navigates to data tracks via sidebar.
      const audio = buildAudioEntries(tracks);
      const audioCount = audio.filter((e) => !e.is_data).length;
      if (audio.length > 0) {
        navIdRef.current++;
        setAudioEntries(audio);
        setEntries([]);
        setViewMode("audio");
        setStatusText(`${audioCount} audio track${audioCount !== 1 ? "s" : ""}${audio.length > audioCount ? `, ${audio.length - audioCount} data track` : ""}`);
      } else {
        // Data-only disc: load the filesystem immediately.
        await loadDirectory(path, "/");
      }
    } else {
      setCueTracks([]);
      setSidebarPath("/");

      const filesystems = await invoke<string[]>("get_disc_filesystems", { imagePath: path }).catch(() => ["ISO 9660"]);
      const makeFsNode = (fs: string): TreeNode => ({
        name: fs,
        path: `__fs_${fs.toLowerCase().replace(/ /g, "_")}`,
        nodeType: "filesystem" as NodeType,
        children: null,
        expanded: false,
      });

      const fsChildren = filesystems.map(makeFsNode);
      const rootNode: TreeNode = { name, path: "__root", nodeType: "root", children: fsChildren, expanded: true };
      setTree([rootNode]);
      const firstFs = filesystems[0] ?? "ISO 9660";
      const firstFsPath = `__fs_${firstFs.toLowerCase().replace(/ /g, "_")}`;
      setSidebarPath(firstFsPath);
      await loadDirectory(path, "/", firstFs, firstFs);
      try {
        const result = await invoke<DiscEntry[]>("list_disc_contents", { imagePath: path, dirPath: "/", filesystem: firstFs });
        const subDirs = result.filter((e) => e.is_dir)
          .map((e): TreeNode => ({ name: e.name, path: `/${e.name}`, nodeType: "dir", children: null, expanded: false }));
        setTree([{ ...rootNode, children: fsChildren.map((c) => c.path === firstFsPath ? { ...c, expanded: true, children: subDirs } : c) }]);
      } catch { /* tree update failed */ }
    }
  }

  async function openImage() {
    const selected = await open({
      filters: [{ name: "Disc Images", extensions: ["iso", "img", "chd", "cue", "mds", "mdx", "nrg", "ccd", "cdi", "gdi", "toc", "b6t", "bwt", "c2d", "pdi", "gi", "daa", "cso", "ciso", "ecm"] }],
    });
    if (!selected) return;
    await openImageAtPath(selected as string);
  }

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    getCurrentWebview().onDragDropEvent((event) => {
      if (event.payload.type === "drop") {
        setIsDragOver(false);
        const supported = ["iso", "img", "chd", "cue", "mds", "mdx", "nrg", "ccd", "cdi", "gdi", "toc", "b6t", "bwt", "c2d", "pdi", "gi", "daa", "cso", "ciso", "ecm"];
        const path = event.payload.paths.find((p) =>
          supported.some((ext) => p.toLowerCase().endsWith(`.${ext}`))
        );
        if (path) openImageAtPath(path);
      } else if (event.payload.type === "leave") {
        setIsDragOver(false);
      } else {
        setIsDragOver(true);
      }
    }).then((fn) => { unlisten = fn; });
    return () => { unlisten?.(); };
  }, []);

  async function mountImage() {
    if (!sourceImagePath) return;
    try {
      const result = await invoke<MountResult>("mount_disc_image", { imagePath: sourceImagePath });
      setMountedDevice(result.device);
      setError(null);
      // Keep browsing via the source image using our own readers — macOS may not
      // be able to read the mounted filesystem (e.g. UDF 2.50 is unsupported).
      // The mount makes the device visible in Finder/Disk Utility.
      const name = imageName; // already set from the source image
      const rootNode: TreeNode = { name, path: "/", nodeType: "root", children: null, expanded: false };
      setTree([rootNode]);
      await loadDirectory(sourceImagePath, "/");
      const entries2 = await invoke<DiscEntry[]>("list_disc_contents", { imagePath: sourceImagePath, dirPath: "/" });
      const subDirs = entries2.filter(e => e.is_dir).map(e => ({
        name: e.name, path: `/${e.name}`, nodeType: "dir" as NodeType, children: null, expanded: false,
      }));
      setTree([{ ...rootNode, expanded: true, children: subDirs }]);
    } catch (e) {
      setError(String(e));
    }
  }

  async function unmountImage() {
    if (!mountedDevice) return;
    try {
      await invoke("unmount_disc_image", { device: mountedDevice });
    } catch (e) {
      setError(String(e));
    }
    setMountedDevice(null);
    // Keep the disc image open in the app after unmounting.
  }

  function isCdemuEmulatable(path: string): boolean {
    const lower = path.toLowerCase();
    return [".iso", ".img", ".cue", ".mds", ".mdx", ".nrg", ".ccd", ".cdi",
            ".gdi", ".toc", ".b6t", ".bwt", ".c2d", ".pdi", ".gi", ".daa"]
      .some(ext => lower.endsWith(ext));
  }

  async function emulateDrive() {
    if (!sourceImagePath) return;
    setEmulating(true);
    try {
      const drive = await invoke<EmulatedDrive>("emulate_drive", { imagePath: sourceImagePath });
      setEmulatedDrives(prev => [...prev, drive]);
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setEmulating(false);
    }
  }

  async function ejectEmulatedDrive(slot: string) {
    try {
      await invoke("eject_emulated_drive", { slot });
      setEmulatedDrives(prev => prev.filter(d => d.slot !== slot));
    } catch (e) {
      setError(String(e));
    }
  }

  function unmountPhysicalDisc() {
    setPhysicalDiscActive(false);
    setImagePath(null);
    setImageName("");
    setEntries([]);
    setAudioEntries([]);
    setTree([]);
    setCueTracks([]);
    setActiveFilesystem("");
    setSidebarPath("");
    setError(null);
    setStatusText("No disc loaded");
    setViewMode("filesystem");
  }

  async function ejectDisc() {
    if (!imagePath) return;
    try {
      await invoke("eject_disc", { path: imagePath });
    } catch (e) {
      setError(String(e));
    }
    unmountPhysicalDisc();
  }

  async function openDisc() {
    setLoadingDrives(true);
    setShowDriveMenu(true);
    try {
      const result = await invoke<DriveInfo[]>("list_optical_drives");
      setDrives(result);
    } catch (e) {
      setError(String(e));
      setShowDriveMenu(false);
    } finally {
      setLoadingDrives(false);
    }
  }

  async function selectDrive(drive: DriveInfo) {
    setShowDriveMenu(false);
    setError(null);

    if (!drive.has_disc) {
      setViewMode("empty-drive");
      setEmptyDriveName(drive.name);
      setSourceImagePath(null);
      setImagePath(null);
      setImageName("");
      setEntries([]);
      setAudioEntries([]);
      setTree([]);
      setStatusText("No disc loaded");
      setPhysicalDiscActive(false);
      return;
    }

    const name = drive.volume_name || drive.name;
    setSourceImagePath(null);
    setPhysicalDiscActive(true);
    setImagePath(drive.device_path);
    setImageName(name);
    setEmptyDriveName(null);

    const rootNode: TreeNode = { name, path: "/", nodeType: "root", children: null, expanded: false };
    setTree([rootNode]);
    await loadDirectory(drive.device_path, "/");

    try {
      const result = await invoke<DiscEntry[]>("list_disc_contents", {
        imagePath: drive.device_path, dirPath: "/",
      });
      const subDirs = result
        .filter((e) => e.is_dir)
        .map((e): TreeNode => ({
          name: e.name, path: `/${e.name}`, nodeType: "dir", children: null, expanded: false,
        }));
      setTree([{ ...rootNode, expanded: true, children: subDirs }]);
    } catch {
      // Tree build failed; directory already loaded above
    }
  }

  async function dumpContents() {
    if (!imagePath) return;
    const destPath = await open({ directory: true, title: "Choose destination for disc contents" });
    if (!destPath) return;
    const volName = (tree[0]?.name ?? imageName).replace(/\.[^/.]+$/, "") || "disc";
    try {
      await invoke("save_directory", {
        imagePath,
        dirPath: "/",
        destPath: `${destPath}/${volName}`,
        filesystem: activeFilesystem || null,
      });
    } catch (e) { setError(String(e)); }
  }

  async function dumpDisc() {
    if (!imagePath) return;
    const destPath = await open({ directory: true, title: "Choose destination for disc dump" });
    if (!destPath) return;
    const discName = imageName.replace(/\.[^/.]+$/, "") || "disc";
    try {
      await invoke("save_directory", {
        imagePath,
        dirPath: "/",
        destPath: `${destPath}/${discName}`,
      });
    } catch (e) {
      setError(String(e));
    }
  }

  async function handleTreeToggle(nodePath: string) {
    if (!imagePath) return;

    if (nodePath.startsWith("__track_")) {
      function toggleExpanded(nodes: TreeNode[]): TreeNode[] {
        return nodes.map((n) => {
          if (n.path === nodePath) return { ...n, expanded: !n.expanded };
          if (n.children) return { ...n, children: toggleExpanded(n.children) };
          return n;
        });
      }
      setTree(toggleExpanded(tree));
      return;
    }

    if (nodePath.startsWith("__fs_") && cueTracks.length === 0) {
      function toggleFs(nodes: TreeNode[]): TreeNode[] {
        return nodes.map((n) => {
          if (n.path === nodePath) return { ...n, expanded: !n.expanded };
          if (n.children) return { ...n, children: toggleFs(n.children) };
          return n;
        });
      }
      setTree(toggleFs(tree));
      return;
    }

    if (nodePath.startsWith("__")) return;

    async function expandNode(nodes: TreeNode[]): Promise<TreeNode[]> {
      return Promise.all(nodes.map(async (node) => {
        if (node.path !== nodePath) {
          return { ...node, children: node.children ? await expandNode(node.children) : null };
        }
        if (node.expanded) return { ...node, expanded: false };
        let children = node.children;
        if (children === null) {
          const result = await invoke<DiscEntry[]>("list_disc_contents", { imagePath, dirPath: nodePath });
          children = result
            .filter((e) => e.is_dir)
            .map((e): TreeNode => ({
              name: e.name,
              path: nodePath === "/" ? `/${e.name}` : `${nodePath}/${e.name}`,
              nodeType: "dir",
              children: null,
              expanded: false,
            }));
        }
        return { ...node, expanded: true, children };
      }));
    }
    setTree(await expandNode(tree));
  }

  function findNodeByPath(nodes: TreeNode[], target: string): TreeNode | null {
    for (const n of nodes) {
      if (n.path === target) return n;
      if (n.children) {
        const found = findNodeByPath(n.children, target);
        if (found) return found;
      }
    }
    return null;
  }

  function handleTreeSelect(path: string) {
    if (!imagePath) return;

    if (path === "__root") {
      setSidebarPath("__root");
      const audio = buildAudioEntries(cueTracks);
      const audioCount = audio.filter((e) => !e.is_data).length;
      if (audio.length > 0) {
        navIdRef.current++;
        setAudioEntries(audio);
        setEntries([]);
        setViewMode("audio");
        setCurrentPath("__root");
        setStatusText(`${audioCount} audio track${audioCount !== 1 ? "s" : ""}${audio.length > audioCount ? `, ${audio.length - audioCount} data track` : ""}`);
      } else {
        loadDirectory(imagePath, "/");
      }
      return;
    }

    if (path.startsWith("__session_")) {
      setSidebarPath(path);
      const sessionNum = parseInt(path.replace("__session_", ""), 10);
      const sessionTracks = cueTracks.filter((t) => t.session === sessionNum);
      navIdRef.current++;
      const audio = buildAudioEntries(sessionTracks);
      const audioCount = audio.filter((e) => !e.is_data).length;
      setAudioEntries(audio);
      setEntries([]);
      setViewMode("audio");
      setCurrentPath(path);
      setStatusText(`Session ${sessionNum} — ${audioCount} audio track${audioCount !== 1 ? "s" : ""}${audio.length > audioCount ? `, ${audio.length - audioCount} data track` : ""}`);
      return;
    }

    if (path.startsWith("__audio_")) {
      setSidebarPath(path);
      const trackNum = parseInt(path.replace("__audio_", ""), 10);
      const track = cueTracks.find((t) => t.number === trackNum && !t.is_data);
      if (track) {
        navIdRef.current++;
        const audio = buildAudioEntries([track]);
        setAudioEntries(audio);
        setEntries([]);
        setViewMode("audio");
        setCurrentPath(path);
        setStatusText(`Track ${String(track.number).padStart(2, "0")} — ${track.mode}`);
      }
      return;
    }

    if (path.startsWith("__track_")) {
      setSidebarPath(path);
      return;
    }

    if (path.startsWith("__fs_")) {
      setSidebarPath(path);
      const fsName = findNodeByPath(tree, path)?.name ?? "";
      loadDirectory(imagePath, "/", fsName, fsName);
      // For non-CUE discs: expand clicked filesystem node with its subdirs,
      // collapsing all sibling filesystem nodes.
      if (cueTracks.length === 0) {
        invoke<DiscEntry[]>("list_disc_contents", { imagePath, dirPath: "/", filesystem: fsName })
          .then((result) => {
            const subDirs = result.filter((e) => e.is_dir)
              .map((e): TreeNode => ({ name: e.name, path: `/${e.name}`, nodeType: "dir", children: null, expanded: false }));
            setTree((prev) => {
              function swapFs(nodes: TreeNode[]): TreeNode[] {
                return nodes.map((n) => {
                  if (n.nodeType === "filesystem") {
                    return n.path === path
                      ? { ...n, expanded: true, children: subDirs }
                      : { ...n, expanded: false, children: null };
                  }
                  if (n.children) return { ...n, children: swapFs(n.children) };
                  return n;
                });
              }
              return swapFs(prev);
            });
          })
          .catch(() => {});
      }
      return;
    }

    if (!path.startsWith("__")) loadDirectory(imagePath, path);
  }

  async function saveEntry(entry: DiscEntry) {
    if (!imagePath) return;
    const entryPath = currentPath === "/" ? `/${entry.name}` : `${currentPath}/${entry.name}`;

    if (entry.is_dir) {
      const base = defaultDownloadPath || await open({ directory: true, title: `Choose destination for "${entry.name}"` }) as string | null;
      if (!base) return;
      try {
        await invoke("save_directory", { imagePath, dirPath: entryPath, destPath: `${base}/${entry.name}`, filesystem: activeFilesystem || null });
      } catch (e) { setError(String(e)); }
    } else {
      const destPath = defaultDownloadPath
        ? `${defaultDownloadPath}/${entry.name}`
        : await save({ defaultPath: entry.name });
      if (!destPath) return;
      try {
        await invoke("save_file", { imagePath, filePath: entryPath, destPath, filesystem: activeFilesystem || null });
      } catch (e) { setError(String(e)); }
    }
  }

  async function saveAudioTrack(entry: AudioEntry) {
    if (!imagePath) return;
    const ext = audioFormat;
    const destPath = defaultDownloadPath
      ? `${defaultDownloadPath}/${entry.name}.${ext}`
      : await save({
          defaultPath: `${entry.name}.${ext}`,
          filters: [{ name: ext === "flac" ? "FLAC Audio" : ext === "mp3" ? "MP3 Audio" : "WAV Audio", extensions: [ext] }],
        });
    if (!destPath) return;
    try {
      await invoke("save_audio_track", {
        cuePath: imagePath,
        trackNumber: entry.track_number,
        destPath,
        format: ext,
      });
    } catch (e) { setError(String(e)); }
  }

  function navigateUp() {
    if (!imagePath || currentPath === "/" || viewMode === "audio") return;
    const parent = currentPath.substring(0, currentPath.lastIndexOf("/")) || "/";
    loadDirectory(imagePath, parent);
  }

  const breadcrumbs = currentPath === "/" || viewMode === "audio"
    ? [{ label: imageName || "Root", path: "/" }]
    : [
        { label: imageName || "Root", path: "/" },
        ...currentPath.split("/").filter(Boolean).map((part, i, arr) => ({
          label: part,
          path: "/" + arr.slice(0, i + 1).join("/"),
        })),
      ];

  const fsCols: { key: keyof ColWidths; label: string }[] = [
    { key: "name", label: "Name" },
    { key: "lba", label: "LBA" },
    { key: "size", label: "Size" },
    { key: "modified", label: "Modified" },
    { key: "save", label: "Save" },
  ];

  const showAudioSave = audioEntries.some(e => !e.is_data);

  const audioCols: { key: keyof ColWidths; label: string }[] = [
    { key: "name", label: "Track" },
    { key: "lba", label: "Start Sector" },
    { key: "size", label: "Duration" },
    { key: "modified", label: "Format" },
    ...(showAudioSave ? [{ key: "save" as keyof ColWidths, label: "Save" }] : []),
  ];

  const cols = viewMode === "audio" ? audioCols : fsCols;

  if (IS_SECTOR_VIEW_WINDOW) {
    if (!svParams) return null;
    return (
      <SectorView
        imagePath={svParams.imagePath}
        initialLba={svParams.lba}
        onClose={() => getCurrentWindow().close()}
        standalone
      />
    );
  }

  return (
    <div className="app">
      {isDragOver && (
        <div className="drag-overlay">
          <div className="drag-overlay-inner">
            <div className="drag-overlay-icon">💿</div>
            <p>Drop disc image to open</p>
          </div>
        </div>
      )}
      <div className="toolbar">
        <div className="toolbar-left" />
        <div className="toolbar-center">
          <button className="btn-open" onClick={openImage}>Open Disc Image</button>
          {mountedDevice
            ? <button className="btn-open btn-open-secondary btn-unmount" onClick={unmountImage}>Unmount Disc Image</button>
            : sourceImagePath && isMountable(sourceImagePath, platform)
              ? <button className="btn-open btn-open-secondary" onClick={mountImage}>Mount Disc Image</button>
              : null
          }
          {platform === "linux" && sourceImagePath && isCdemuEmulatable(sourceImagePath) && (
            <button className="btn-open btn-open-secondary" onClick={emulateDrive} disabled={emulating}>
              {emulating ? "Loading…" : "Emulate Drive"}
            </button>
          )}
          <div className="separator" />
          <div className="drive-menu-wrap" ref={driveMenuRef}>
            {physicalDiscActive
              ? <>
                  <button className="btn-open btn-open-secondary btn-unmount" onClick={unmountPhysicalDisc}>Unmount Disc</button>
                  <button className="btn-open btn-open-secondary btn-unmount btn-eject" onClick={ejectDisc} title="Eject disc">⏏</button>
                </>
              : <button className="btn-open btn-open-secondary" onClick={openDisc}>Open Disc from Drive ▾</button>
            }
            {showDriveMenu && (
              <div className="drive-menu">
                {loadingDrives ? (
                  <div className="drive-menu-item drive-menu-loading">Detecting drives…</div>
                ) : drives.length === 0 ? (
                  <div className="drive-menu-item drive-menu-empty">No optical drives found</div>
                ) : (
                  drives.map((d) => (
                    <div key={d.device_path} className="drive-menu-item" onClick={() => selectDrive(d)}>
                      <span className="drive-item-name">{d.name}</span>
                      <span className={`drive-item-disc ${d.has_disc ? "" : "drive-item-disc--empty"}`}>
                        {d.has_disc ? (d.volume_name || "Disc inserted") : "No disc"}
                      </span>
                    </div>
                  ))
                )}
              </div>
            )}
          </div>
          {mountedDevice && (
            <>
              <div className="separator" />
              <button className="btn-dump" onClick={dumpDisc} title="Extract all disc contents to a folder">
                Dump Disc
              </button>
            </>
          )}
          {imagePath && viewMode === "filesystem" && (
            <>
              <div className="separator" />
              <button className="btn-dump" onClick={dumpContents} title="Extract all disc contents to a folder">
                Extract All Contents
              </button>
            </>
          )}
          {imagePath && viewMode === "filesystem" && (
            <>
              <div className="separator" />
              <button className="btn-icon" onClick={navigateUp} disabled={currentPath === "/"} title="Up">↑</button>
            </>
          )}
          {sourceImagePath && (
            <>
              <div className="separator" />
              <button className="btn-icon" onClick={() => setShowSectorView(true)} title="Sector View">🔍</button>
            </>
          )}
        </div>
        <div className="toolbar-right">
          <a className="btn-prerelease" href="https://github.com/whatev-indus/disc-xplorer/releases" target="_blank" rel="noreferrer">PRE-RELEASE BUILD! CLICK TO UPDATE</a>
          <button ref={settingsGearRef} className="btn-settings" title="Settings" onClick={() => setShowSettings(s => !s)}>⚙</button>
        </div>
      </div>
      {showSettings && (
        <div className="settings-panel" ref={settingsRef}>
          <div className="settings-row">
            <span className="settings-label">Default Download Location</span>
            <button className="btn-open btn-open-secondary settings-path-btn" onClick={pickDownloadLocation}>
              {defaultDownloadPath || "Not set — click to choose"}
            </button>
          </div>
          <div className="settings-row">
            <span className="settings-label">Save Audio (PCM) as</span>
            <div className="settings-radio-group">
              {(["wav", "flac", "mp3"] as const).map(fmt => (
                <label key={fmt} className="settings-radio">
                  <input type="radio" name="audioFormat" value={fmt} checked={audioFormat === fmt} onChange={() => setAudioFormat(fmt)} />
                  .{fmt}
                </label>
              ))}
            </div>
          </div>
          <div className="settings-row">
            <span className="settings-label">Open Source Notices</span>
            <button className="btn-open btn-open-secondary settings-path-btn" onClick={() => setShowLicenses(true)}>
              View licenses
            </button>
          </div>
        </div>
      )}

      {showLicenses && (
        <div className="modal-overlay" onClick={() => setShowLicenses(false)}>
          <div className="modal" onClick={e => e.stopPropagation()}>
            <div className="modal-header">
              <span className="modal-title">Open Source Notices</span>
              <button className="modal-close" onClick={() => setShowLicenses(false)}>✕</button>
            </div>
            <div className="modal-body">
              <p className="license-package">libFLAC — FLAC audio encoding</p>
              <pre className="license-text">{`Copyright (C) 2000-2009  Josh Coalson
Copyright (C) 2011-2016  Xiph.Org Foundation

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

- Redistributions of source code must retain the above copyright
  notice, this list of conditions and the following disclaimer.

- Redistributions in binary form must reproduce the above copyright
  notice, this list of conditions and the following disclaimer in the
  documentation and/or other materials provided with the distribution.

- Neither the name of the Xiph.org Foundation nor the names of its
  contributors may be used to endorse or promote products derived from
  this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
\`\`AS IS'' AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE FOUNDATION
OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.`}</pre>
              <p className="license-package" style={{ marginTop: "16px" }}>LAME — MP3 audio encoding</p>
              <pre className="license-text">{`Copyright (c) 1999-2011 The L.A.M.E. project

LAME is licensed under the GNU Lesser General Public License (LGPL)
version 2 or later. This application is licensed under GPL v3, which
is compatible with and satisfies the requirements of the LGPL.

Source: https://lame.sourceforge.io`}</pre>
              <p className="license-package" style={{ marginTop: "16px" }}>chd-rs — CHD (Compressed Hunks of Data) decompression</p>
              <pre className="license-text">{`Copyright (c) 2022 Ronny Chan

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

1. Redistributions of source code must retain the above copyright
   notice, this list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright
   notice, this list of conditions and the following disclaimer in
   the documentation and/or other materials provided with the
   distribution.

3. Neither the name of the copyright holder nor the names of its
   contributors may be used to endorse or promote products derived
   from this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.`}</pre>
              <p className="license-package" style={{ marginTop: "16px" }}>libflac-sys — Rust bindings for libFLAC</p>
              <pre className="license-text">{`Copyright (c) 2020 Matthias Geier. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

1. Redistributions of source code must retain the above copyright
   notice, this list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright
   notice, this list of conditions and the following disclaimer in
   the documentation and/or other materials provided with the
   distribution.

3. Neither the name of the copyright holder nor the names of its
   contributors may be used to endorse or promote products derived
   from this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.`}</pre>
            </div>
          </div>
        </div>
      )}

      {showCdemuPrompt && (
        <div className="modal-overlay">
          <div className="modal cdemu-modal" onClick={e => e.stopPropagation()}>
            <div className="modal-header">
              <span className="modal-title">CDemu Not Installed</span>
            </div>
            <div className="modal-body">
              <p>CDemu is required to mount certain disc image formats on Linux (.cue, .mds, .nrg, and others).</p>
              <p>Would you like to install it now?</p>
              {cdemuInstalling && <p className="cdemu-status">Installing… (a system password prompt may appear)</p>}
              {cdemuInstallMsg && (
                <p className={cdemuInstallOk ? "cdemu-status cdemu-ok" : "cdemu-status cdemu-err"}>{cdemuInstallMsg}</p>
              )}
            </div>
            <div className="modal-footer">
              {!cdemuInstallOk && (
                <button className="btn-open" onClick={installCdemu} disabled={cdemuInstalling}>
                  {cdemuInstalling ? "Installing…" : "Install"}
                </button>
              )}
              <button className="btn-open btn-open-secondary" onClick={() => setShowCdemuPrompt(false)}>
                {cdemuInstallOk ? "Done" : "Not Now"}
              </button>
            </div>
          </div>
        </div>
      )}

      {showSectorView && sourceImagePath && (
        <SectorView imagePath={sourceImagePath} onClose={() => setShowSectorView(false)} />
      )}

      {emulatedDrives.length > 0 && (
        <div className="emulated-drives-bar">
          {emulatedDrives.map(drive => (
            <div key={drive.slot} className="emulated-drive-item">
              <span className="emulated-drive-device">{drive.device}</span>
              <span className="emulated-drive-name">{drive.image_path.split("/").pop()}</span>
              <button className="btn-eject-emulated" onClick={() => ejectEmulatedDrive(drive.slot)} title="Unload virtual drive">⏏</button>
            </div>
          ))}
        </div>
      )}

      {(imagePath || viewMode === "empty-drive") && (
        <div className="breadcrumb">
          {breadcrumbs.map((crumb, i) => (
            <span key={crumb.path}>
              {i > 0 && <span className="breadcrumb-sep">›</span>}
              <span
                className={`breadcrumb-item ${i === breadcrumbs.length - 1 ? "breadcrumb-item--active" : ""}`}
                onClick={() => imagePath && i < breadcrumbs.length - 1 && loadDirectory(imagePath, crumb.path)}
              >{crumb.label}</span>
            </span>
          ))}
        </div>
      )}

      <div className="main">
        {imagePath && (
          <div className="sidebar">
            {tree.map((node) => (
              <TreeItem key={node.path} node={node} imagePath={imagePath}
                selectedPath={sidebarPath} onSelect={handleTreeSelect}
                onToggle={handleTreeToggle} depth={0} />
            ))}
          </div>
        )}

        <div className="content">
          {error && <div className="error">{error}</div>}

          {!imagePath && viewMode !== "empty-drive" && (
            <div className="empty-state">
              <div className="empty-icon">💿</div>
              <p>Open a Disc Image or insert a Disc into a Disc Drive to explore its contents.</p>
              <button onClick={openImage}>Open Disc Image</button>
            </div>
          )}

          {viewMode === "empty-drive" && emptyDriveName && (
            <div className="empty-state">
              <div className="empty-icon">📀</div>
              <p>Optical disc drive is empty</p>
              <span className="empty-drive-name">{emptyDriveName}</span>
            </div>
          )}

          {(viewMode === "filesystem" ? entries.length > 0 : audioEntries.length > 0) && (
            <table className="file-table" style={{ tableLayout: "fixed" }}>
              <colgroup>
                {cols.map((c) => <col key={c.key} style={{ width: colWidths[c.key] }} />)}
              </colgroup>
              <thead>
                <tr>
                  {cols.map((c) => (
                    <th key={c.key} className={`col-${c.key}`}>
                      <span className="th-label">{c.label}</span>
                      <div className="resize-handle" onMouseDown={(e) => onResizeStart(c.key, e)} />
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {viewMode === "audio"
                  ? audioEntries.map((entry) => (
                      <tr
                        key={entry.track_number}
                        className={entry.is_data ? "row-data" : "row-audio"}
                        onDoubleClick={() => entry.is_data && imagePath && loadDirectory(imagePath, "/")}
                      >
                        <td className="col-name">
                          <span className="entry-icon">{entry.is_data ? "💿" : "🎵"}</span>
                          {entry.name}
                        </td>
                        <td className="col-lba">{entry.start_lba.toLocaleString()}</td>
                        <td className="col-size">{entry.is_data ? formatSize(entry.size_bytes) : formatDuration(entry.num_sectors)}</td>
                        <td className="col-modified">{entry.format}</td>
                        {showAudioSave && (
                          <td className="col-save">
                            {!entry.is_data && <button className="btn-save" title="Save as WAV" onClick={() => saveAudioTrack(entry)}>⬇</button>}
                          </td>
                        )}
                      </tr>
                    ))
                  : entries.map((entry) => (
                      <tr
                        key={`${entry.lba}-${entry.name}`}
                        className={entry.is_dir ? "row-dir" : "row-file"}
                        onDoubleClick={() => {
                          if (!entry.is_dir || !imagePath) return;
                          const newPath = currentPath === "/" ? `/${entry.name}` : `${currentPath}/${entry.name}`;
                          loadDirectory(imagePath, newPath);
                        }}
                      >
                        <td className="col-name">
                          <span className="entry-icon">{entry.is_dir ? "📁" : "📄"}</span>
                          {entry.name}
                        </td>
                        <td className="col-lba">{entry.lba}</td>
                        <td className="col-size">{entry.size_bytes > 0 ? entry.size_bytes.toLocaleString() : "—"}</td>
                        <td className="col-modified">{entry.modified}</td>
                        <td className="col-save">
                          <button className="btn-save" title={entry.is_dir ? "Save folder" : "Save file"} onClick={() => saveEntry(entry)}>⬇</button>
                        </td>
                      </tr>
                    ))
                }
              </tbody>
            </table>
          )}

          {imagePath && viewMode === "filesystem" && entries.length === 0 && !error && (
            <div className="empty-dir">Empty folder</div>
          )}
        </div>
      </div>

      <div className="statusbar">
        <span className="statusbar-left">{statusText}</span>
        <a className="statusbar-brand" href="https://sites.google.com/view/whateverindustries/home" target="_blank" rel="noreferrer">whatev.indus</a>
        <span className="statusbar-right">
          {updateVersion && (
            <a className="statusbar-update" href="https://github.com/whatev-indus/disc-xplorer/releases" target="_blank" rel="noreferrer">
              v{updateVersion} available
            </a>
          )}
          <button className="statusbar-version" onClick={checkForUpdate} title="Check for updates">v0.2.2</button>
        </span>
      </div>
    </div>
  );
}

export default App;
