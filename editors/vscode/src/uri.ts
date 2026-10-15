import * as vscode from 'vscode';

/** URI scheme for read-only file contents at a revision. */
export const OAK_SCHEME = 'oak';

/** The symbolic ref that tracks the repository's current HEAD commit. */
export const HEAD_REF = 'HEAD';

export interface OakUriParams {
  /** Absolute filesystem path of the file in the working tree. */
  path: string;
  /** `HEAD` or a full commit hash. */
  ref: string;
}

/**
 * Build an `oak:` URI for `uri`'s contents at `ref`. The path part mirrors the
 * file path so editors pick the right language mode and title.
 */
export function toOakUri(uri: vscode.Uri, ref: string): vscode.Uri {
  const params: OakUriParams = { path: uri.fsPath, ref };
  return uri.with({ scheme: OAK_SCHEME, query: JSON.stringify(params) });
}

export function fromOakUri(uri: vscode.Uri): OakUriParams {
  return JSON.parse(uri.query) as OakUriParams;
}

export function shortHash(hash: string | null | undefined): string {
  return hash ? hash.slice(0, 8) : '';
}
