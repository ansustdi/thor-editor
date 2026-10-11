// Load Monaco from the local bundle (the app must work offline) with only the SQL languages.
import { loader } from "@monaco-editor/react";
import * as monaco from "monaco-editor/editor/editor.api";
import "./monaco-contribs";
import EditorWorker from "monaco-editor/editor/editor.worker?worker";
import "monaco-editor/languages/definitions/sql/register";
import "monaco-editor/languages/definitions/mysql/register";
import "monaco-editor/languages/definitions/pgsql/register";
import "monaco-editor/languages/definitions/redis/register";
import "monaco-editor/languages/definitions/cypher/register";
import "monaco-editor/languages/definitions/javascript/register";
import "monaco-editor/languages/definitions/java/register";

self.MonacoEnvironment = { getWorker: () => new EditorWorker() };
loader.config({ monaco });

export { monaco };
