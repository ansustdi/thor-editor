import { Component, type ReactNode } from "react";

/** Contains a crash to one tab/panel instead of blanking the whole app. */
export class ErrorBoundary extends Component<{ children: ReactNode }, { error: Error | null }> {
  state = { error: null as Error | null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  render() {
    if (!this.state.error) return this.props.children;
    return (
      <div className="m-4 space-y-2 rounded-md border border-danger/40 bg-danger/5 p-4 text-[13px]">
        <div className="font-semibold text-danger">This view crashed</div>
        <pre className="whitespace-pre-wrap font-mono text-xs select-text">{this.state.error.message}</pre>
        <button className="rounded-md border border-line px-3 py-1 hover:bg-hover" onClick={() => this.setState({ error: null })}>
          Try again
        </button>
      </div>
    );
  }
}
