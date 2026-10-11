import { invoke } from "@tauri-apps/api/core";

/** Writes a UTF-8 text file chosen by the user through a save dialog. */
export const writeTextFile = (path: string, content: string) => invoke<void>("write_text_file", { path, content });
