// Minimal ambient typing for the DBX Host Bridge surface (window.dbxPlugin).
// Only members listed in the platform fact sheet are declared here.

export interface DbxTheme {
  appearance: "light" | "dark";
  tokens: Record<string, string>;
  editor?: Record<string, unknown>;
}

export interface BinaryEnvelope {
  channel: string;
  data: Uint8Array;
}

export interface DbxPluginBridge {
  ready: Promise<void>;
  contributionId?: string;
  context: Record<string, unknown>;
  locale: string;
  theme: DbxTheme;
  capabilities: Record<string, boolean | undefined>;
  invoke(method: string, params?: unknown, options?: { timeoutMs?: number }): Promise<unknown>;
  notify(method: string, params?: unknown): void;
  request(method: string, params?: unknown): Promise<unknown>;
  copy?(text: string): Promise<unknown>;
  clipboard?: {
    writeText(text: string): Promise<unknown>;
    readText(): Promise<string>;
  };
  sendBinary(channel: string, data: Uint8Array): void;
  onBinary(handler: (envelope: BinaryEnvelope) => void): () => void;
  onContext(handler: (context: Record<string, unknown>) => void): () => void;
  onEvent(handler: (event: { method: string; params: unknown }) => void): () => void;
  openWorkbench(id: string, context?: Record<string, unknown>): Promise<void>;
}

declare global {
  interface Window {
    dbxPlugin?: DbxPluginBridge;
  }
  interface WindowEventMap {
    "dbx-plugin-env": CustomEvent<{ locale?: string; theme?: DbxTheme }>;
  }
}
