// Thin wrapper around the `oak` CLI. Every repository operation in the
// extension goes through `Oak.exec`, which runs the binary non-interactively
// (no TTY, no colour, no progress animation, no update check) and maps the
// documented exit codes onto `OakError.code`.

import * as cp from 'child_process';

/** Exit codes documented in `oak --help`. */
export const enum OakExitCode {
  Success = 0,
  Generic = 1,
  Usage = 2,
  Locked = 3,
  DirtyTree = 4,
  Conflicts = 5,
  Network = 6,
  MergePredictionUncertified = 7,
  IntegrityInconclusive = 8,
}

export interface ExecResult {
  stdout: string;
  stderr: string;
  exitCode: number;
}

export interface ExecOptions {
  cwd: string;
  /** Written to the child's stdin (e.g. `oak desc --file -`). */
  input?: string;
  /** Resolve instead of throwing on a non-zero exit. */
  allowFailure?: boolean;
  /** Kill the child when this fires. */
  cancel?: { onCancellationRequested(listener: () => void): { dispose(): void } };
  /** Extra environment for this invocation. */
  env?: Record<string, string>;
  /** Report each stderr line as it arrives (progress for long operations). */
  onStderrLine?: (line: string) => void;
}

export class OakError extends Error {
  constructor(
    readonly args: string[],
    readonly exitCode: number,
    readonly stdout: string,
    readonly stderr: string,
  ) {
    super(OakError.describe(exitCode, stdout, stderr));
    this.name = 'OakError';
  }

  /** The most useful human-readable line(s) from a failed invocation. */
  static describe(exitCode: number, stdout: string, stderr: string): string {
    const fromJson = errorFromJson(stdout) ?? errorFromJson(stderr);
    if (fromJson) {
      return fromJson;
    }
    const text = stripAnsi(stderr.trim() || stdout.trim());
    const lines = text.split(/\r?\n/).filter((l) => l.trim().length > 0);
    const errorLine = lines.find((l) => /^\s*(error|Error|✗)/.test(l));
    if (errorLine) {
      const idx = lines.indexOf(errorLine);
      return lines.slice(idx, idx + 4).join('\n').replace(/^\s*(error:|Error:|✗)\s*/, '');
    }
    if (lines.length > 0) {
      return lines.slice(-4).join('\n');
    }
    return `oak exited with code ${exitCode}`;
  }

  get isNetwork(): boolean {
    return this.exitCode === OakExitCode.Network;
  }
  get isConflict(): boolean {
    return this.exitCode === OakExitCode.Conflicts;
  }
  get isLocked(): boolean {
    return this.exitCode === OakExitCode.Locked;
  }
  get isDirtyTree(): boolean {
    return this.exitCode === OakExitCode.DirtyTree;
  }
}

function errorFromJson(text: string): string | undefined {
  const trimmed = text.trim();
  if (!trimmed.startsWith('{')) {
    return undefined;
  }
  try {
    const doc = JSON.parse(trimmed);
    const msg = doc.error?.message ?? doc.message ?? (typeof doc.error === 'string' ? doc.error : undefined);
    return typeof msg === 'string' && msg.length > 0 ? msg : undefined;
  } catch {
    return undefined;
  }
}

export function stripAnsi(text: string): string {
  // eslint-disable-next-line no-control-regex
  return text.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, '');
}

export type Logger = (line: string) => void;

export class Oak {
  constructor(
    readonly path: string,
    private readonly log: Logger = () => undefined,
  ) {}

  exec(args: string[], options: ExecOptions): Promise<ExecResult> {
    const started = Date.now();
    this.log(`> oak ${args.map(quoteArg).join(' ')}`);
    return new Promise<ExecResult>((resolve, reject) => {
      let child: cp.ChildProcess;
      try {
        child = cp.spawn(this.path, args, {
          cwd: options.cwd,
          env: {
            ...process.env,
            NO_COLOR: '1',
            OAK_PROGRESS: 'never',
            OAK_NO_UPDATE_CHECK: '1',
            PAGER: 'cat',
            GIT_TERMINAL_PROMPT: '0',
            ...options.env,
          },
          stdio: ['pipe', 'pipe', 'pipe'],
          windowsHide: true,
        });
      } catch (err) {
        reject(err);
        return;
      }

      const stdout: Buffer[] = [];
      const stderr: Buffer[] = [];
      let stderrPartial = '';
      child.stdout!.on('data', (b: Buffer) => stdout.push(b));
      child.stderr!.on('data', (b: Buffer) => {
        stderr.push(b);
        if (options.onStderrLine) {
          stderrPartial += b.toString('utf8');
          const parts = stderrPartial.split(/\r?\n|\r/);
          stderrPartial = parts.pop() ?? '';
          for (const p of parts) {
            const line = stripAnsi(p).trim();
            if (line) {
              options.onStderrLine(line);
            }
          }
        }
      });

      const sub = options.cancel?.onCancellationRequested(() => child.kill());

      child.on('error', (err: NodeJS.ErrnoException) => {
        sub?.dispose();
        if (err.code === 'ENOENT') {
          reject(new OakNotFoundError(this.path));
        } else {
          reject(err);
        }
      });

      child.on('close', (code) => {
        sub?.dispose();
        const result: ExecResult = {
          stdout: Buffer.concat(stdout).toString('utf8'),
          stderr: Buffer.concat(stderr).toString('utf8'),
          exitCode: code ?? -1,
        };
        this.log(`< exit ${result.exitCode} in ${Date.now() - started}ms`);
        if (result.exitCode !== 0) {
          const tail = stripAnsi(result.stderr).trim();
          if (tail) {
            this.log(tail.split(/\r?\n/).slice(-20).join('\n'));
          }
        }
        if (result.exitCode !== 0 && !options.allowFailure) {
          reject(new OakError(args, result.exitCode, result.stdout, result.stderr));
        } else {
          resolve(result);
        }
      });

      if (options.input !== undefined) {
        child.stdin!.end(options.input, 'utf8');
      } else {
        child.stdin!.end();
      }
    });
  }

  /** Run and parse stdout as a single JSON document. */
  async json<T>(args: string[], options: ExecOptions): Promise<T> {
    const result = await this.exec(args, options);
    return parseJsonDocument<T>(result.stdout, args);
  }

  async version(): Promise<string> {
    const { stdout } = await this.exec(['--version'], { cwd: process.cwd() });
    const m = /(\d+\.\d+\.\d+)/.exec(stdout);
    return m ? m[1] : stdout.trim();
  }
}

export class OakNotFoundError extends Error {
  constructor(readonly path: string) {
    super(`The oak CLI was not found at "${path}". Install it from https://oak.space or set "oak.path".`);
    this.name = 'OakNotFoundError';
  }
}

/**
 * Parse a JSON document from CLI stdout. Some commands print human-readable
 * lines before the JSON payload; take the last line that parses, falling back
 * to the first `{`/`[` to the end.
 */
export function parseJsonDocument<T>(stdout: string, args: string[] = []): T {
  const text = stdout.trim();
  try {
    return JSON.parse(text) as T;
  } catch {
    // fall through
  }
  const lines = text.split(/\r?\n/);
  for (let i = lines.length - 1; i >= 0; i--) {
    const line = lines[i].trim();
    if (line.startsWith('{') || line.startsWith('[')) {
      try {
        return JSON.parse(lines.slice(i).join('\n')) as T;
      } catch {
        // keep looking
      }
    }
  }
  throw new Error(`oak ${args.join(' ')}: expected JSON output, got: ${text.slice(0, 200)}`);
}

function quoteArg(a: string): string {
  return /^[\w@%+=:,./-]+$/.test(a) ? a : JSON.stringify(a);
}
