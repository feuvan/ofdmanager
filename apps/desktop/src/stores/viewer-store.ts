import { create } from "zustand";

import {
  closeDocument as closeDocumentCommand,
  chooseDocument,
  choosePdfDestination,
  exportDocumentPdf,
  openDocument,
  openLaunchDocument,
  type DocumentSummary,
  type ExportProgress,
  type ExportResult,
  type SignatureReport,
  verifyDocument,
} from "@/lib/tauri";

export const MIN_ZOOM = 0.4;
export const MAX_ZOOM = 3;
export const ZOOM_STEP = 0.1;

export type FitMode = "page" | "width" | null;
export type SidebarView = "pages" | "outline";

export function clampZoom(zoom: number) {
  return Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, zoom));
}

interface ViewerState {
  document: DocumentSummary | null;
  pageIndex: number;
  zoom: number;
  fitScale: number;
  fitMode: FitMode;
  sidebarView: SidebarView;
  sidebarOpen: boolean;
  inspectorOpen: boolean;
  loading: boolean;
  verifying: boolean;
  verification: SignatureReport[] | null;
  exporting: boolean;
  exportProgress: ExportProgress | null;
  exportResult: ExportResult | null;
  error: string | null;
  initialize: () => Promise<void>;
  openPath: (path: string) => Promise<void>;
  closeDocument: () => Promise<void>;
  goToPage: (pageIndex: number) => void;
  nextPage: () => void;
  previousPage: () => void;
  setZoom: (zoom: number) => void;
  setFitScale: (scale: number) => void;
  zoomIn: () => void;
  zoomOut: () => void;
  setFitMode: (mode: Exclude<FitMode, null>) => void;
  setSidebarView: (view: SidebarView) => void;
  toggleSidebar: () => void;
  toggleInspector: () => void;
  verify: () => Promise<void>;
  exportPdf: (path: string) => Promise<void>;
  setExportProgress: (progress: ExportProgress) => void;
  clearExportResult: () => void;
  openSelectedDocument: () => Promise<void>;
  exportCurrentPdf: () => Promise<void>;
  setError: (error: unknown) => void;
  clearError: () => void;
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

let operationGeneration = 0;

export const useViewerStore = create<ViewerState>((set, get) => ({
  document: null,
  pageIndex: 0,
  zoom: 1,
  fitScale: 1,
  fitMode: "page",
  sidebarView: "pages",
  sidebarOpen: true,
  inspectorOpen: true,
  loading: false,
  verifying: false,
  verification: null,
  exporting: false,
  exportProgress: null,
  exportResult: null,
  error: null,

  openSelectedDocument: async () => {
    try {
      const path = await chooseDocument();
      if (path) {
        await get().openPath(path);
      }
    } catch (error) {
      set({ error: errorMessage(error) });
    }
  },

  initialize: async () => {
    const generation = ++operationGeneration;
    try {
      const document = await openLaunchDocument();
      if (document && generation === operationGeneration) {
        set({
          document,
          pageIndex: 0,
          zoom: 1,
          fitScale: 1,
          fitMode: "page",
        });
      }
    } catch (error) {
      if (generation !== operationGeneration) {
        return;
      }
      set({ error: errorMessage(error) });
    }
  },

  openPath: async (path) => {
    const generation = ++operationGeneration;
    set({
      loading: true,
      exporting: false,
      exportProgress: null,
      error: null,
      verification: null,
    });
    try {
      const document = await openDocument(path);
      if (generation !== operationGeneration) {
        return;
      }
      set({
        document,
        pageIndex: 0,
        zoom: 1,
        fitScale: 1,
        fitMode: "page",
        exportResult: null,
        loading: false,
      });
    } catch (error) {
      if (generation !== operationGeneration) {
        return;
      }
      set({ loading: false, error: errorMessage(error) });
    }
  },

  closeDocument: async () => {
    ++operationGeneration;
    try {
      await closeDocumentCommand();
      set({
        document: null,
        pageIndex: 0,
        zoom: 1,
        fitScale: 1,
        fitMode: "page",
        verification: null,
        exporting: false,
        exportProgress: null,
        exportResult: null,
        error: null,
      });
    } catch (error) {
      set({ error: errorMessage(error) });
    }
  },

  goToPage: (pageIndex) => {
    const pageCount = get().document?.pageCount ?? 0;
    if (pageCount === 0) {
      return;
    }
    set({ pageIndex: Math.min(pageCount - 1, Math.max(0, pageIndex)) });
  },

  nextPage: () => get().goToPage(get().pageIndex + 1),
  previousPage: () => get().goToPage(get().pageIndex - 1),

  setZoom: (zoom) => set({ zoom: clampZoom(zoom), fitMode: null }),
  setFitScale: (fitScale) => {
    if (Math.abs(get().fitScale - fitScale) > 0.001) {
      set({ fitScale });
    }
  },
  zoomIn: () => {
    const state = get();
    state.setZoom((state.fitMode ? state.fitScale : state.zoom) + ZOOM_STEP);
  },
  zoomOut: () => {
    const state = get();
    state.setZoom((state.fitMode ? state.fitScale : state.zoom) - ZOOM_STEP);
  },
  setFitMode: (fitMode) => set({ fitMode }),

  setSidebarView: (sidebarView) => set({ sidebarView }),
  toggleSidebar: () => set((state) => ({ sidebarOpen: !state.sidebarOpen })),
  toggleInspector: () =>
    set((state) => ({ inspectorOpen: !state.inspectorOpen })),

  verify: async () => {
    const generation = operationGeneration;
    set({ verifying: true, error: null });
    try {
      const verification = await verifyDocument();
      if (generation !== operationGeneration) {
        return;
      }
      set({ verifying: false, verification });
    } catch (error) {
      if (generation !== operationGeneration) {
        return;
      }
      set({ verifying: false, error: errorMessage(error) });
    }
  },

  exportPdf: async (path) => {
    const generation = operationGeneration;
    set({
      exporting: true,
      exportProgress: { current: 0, total: get().document?.pageCount ?? 0 },
      exportResult: null,
      error: null,
    });
    try {
      const exportResult = await exportDocumentPdf(path);
      if (generation !== operationGeneration) {
        return;
      }
      set({ exporting: false, exportProgress: null, exportResult });
    } catch (error) {
      if (generation !== operationGeneration) {
        return;
      }
      set({
        exporting: false,
        exportProgress: null,
        error: errorMessage(error),
      });
    }
  },

  exportCurrentPdf: async () => {
    const document = get().document;
    if (!document) {
      return;
    }
    try {
      const path = await choosePdfDestination(document.fileName);
      if (path) {
        await get().exportPdf(path);
      }
    } catch (error) {
      set({ error: errorMessage(error) });
    }
  },

  setExportProgress: (exportProgress) => {
    if (get().exporting) {
      set({ exportProgress });
    }
  },
  clearExportResult: () => set({ exportResult: null }),
  setError: (error) => set({ error: errorMessage(error) }),
  clearError: () => set({ error: null }),
}));
