import { ask } from "@tauri-apps/plugin-dialog";

/**
 * Native Yes/Cancel dialog. Never use window.confirm in this app: Tauri replaces it with an
 * async version whose Promise is always truthy, so destructive actions would run unasked.
 * Fails safe — if the dialog cannot be shown, the answer is "no".
 */
export async function confirmAsk(message: string, opts: { title?: string; danger?: boolean; okLabel?: string } = {}): Promise<boolean> {
  try {
    return await ask(message, {
      title: opts.title ?? "Thor Editor",
      kind: opts.danger ? "warning" : "info",
      okLabel: opts.okLabel ?? "Yes",
      cancelLabel: "Cancel",
    });
  } catch {
    return false;
  }
}
