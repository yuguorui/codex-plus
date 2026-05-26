import { execFile } from 'node:child_process';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { promisify } from 'node:util';

const execFileAsync = promisify(execFile);

const VCS_DIRECTORIES_TO_EXCLUDE = ['.git', '.svn', '.hg', '.bzr', '.jj', '.sl'];
const DEFAULT_HEAD_LIMIT = 250;

async function ripGrep(args, target) {
  try {
    const { stdout } = await execFileAsync('rg', [...args, target], {
      maxBuffer: 20_000_000,
      timeout: 20_000,
      killSignal: process.platform === 'win32' ? undefined : 'SIGKILL',
    });
    return stdout
      .trim()
      .split('\n')
      .map(line => line.replace(/\r$/, ''))
      .filter(Boolean);
  } catch (error) {
    if (error.code === 1) return [];
    throw error;
  }
}

function toRelativePath(filePath) {
  const relative = path.relative(process.cwd(), filePath);
  return relative === '' ? filePath : relative.split(path.sep).join('/');
}

function plural(count, word) {
  return `${word}${count === 1 ? '' : 's'}`;
}

function applyHeadLimit(items, limit, offset = 0) {
  if (limit === 0) {
    return { items: items.slice(offset), appliedLimit: undefined };
  }
  const effectiveLimit = limit ?? DEFAULT_HEAD_LIMIT;
  const sliced = items.slice(offset, offset + effectiveLimit);
  const wasTruncated = items.length - offset > effectiveLimit;
  return {
    items: sliced,
    appliedLimit: wasTruncated ? effectiveLimit : undefined,
  };
}

function formatLimitInfo(appliedLimit, appliedOffset) {
  const parts = [];
  if (appliedLimit !== undefined) parts.push(`limit: ${appliedLimit}`);
  if (appliedOffset) parts.push(`offset: ${appliedOffset}`);
  return parts.join(', ');
}

async function globToolCall(input, globLimits) {
  const limit = globLimits?.maxResults ?? 100;
  const searchDir = input.path ?? process.cwd();
  const args = [
    '--files',
    '--glob',
    input.pattern,
    '--sort=modified',
    '--no-ignore',
    '--hidden',
  ];
  const allPaths = await ripGrep(args, searchDir);
  const absolutePaths = allPaths.map(filePath =>
    path.isAbsolute(filePath) ? filePath : path.join(searchDir, filePath),
  );
  const files = absolutePaths.slice(0, limit);
  return {
    filenames: files.map(toRelativePath),
    durationMs: 0,
    numFiles: files.length,
    truncated: absolutePaths.length > limit,
  };
}

function globToolResult(output, toolUseID) {
  if (output.filenames.length === 0) {
    return {
      tool_use_id: toolUseID,
      type: 'tool_result',
      content: 'No files found',
    };
  }
  return {
    tool_use_id: toolUseID,
    type: 'tool_result',
    content: [
      ...output.filenames,
      ...(output.truncated
        ? ['(Results are truncated. Consider using a more specific path or pattern.)']
        : []),
    ].join('\n'),
  };
}

async function grepToolCall(input) {
  const {
    pattern,
    path: inputPath,
    glob,
    type,
    output_mode = 'files_with_matches',
    '-B': contextBefore,
    '-A': contextAfter,
    '-C': contextC,
    context,
    '-n': showLineNumbers = true,
    '-i': caseInsensitive = false,
    head_limit: headLimit,
    offset = 0,
    multiline = false,
  } = input;
  const absolutePath = inputPath ? path.resolve(inputPath) : process.cwd();
  const args = ['--hidden'];
  for (const dir of VCS_DIRECTORIES_TO_EXCLUDE) {
    args.push('--glob', `!${dir}`);
  }
  args.push('--max-columns', '500');
  if (multiline) args.push('-U', '--multiline-dotall');
  if (caseInsensitive) args.push('-i');
  if (output_mode === 'files_with_matches') args.push('-l');
  else if (output_mode === 'count') args.push('-c');
  if (showLineNumbers && output_mode === 'content') args.push('-n');
  if (output_mode === 'content') {
    if (context !== undefined) args.push('-C', String(context));
    else if (contextC !== undefined) args.push('-C', String(contextC));
    else {
      if (contextBefore !== undefined) args.push('-B', String(contextBefore));
      if (contextAfter !== undefined) args.push('-A', String(contextAfter));
    }
  }
  if (pattern.startsWith('-')) args.push('-e', pattern);
  else args.push(pattern);
  if (type) args.push('--type', type);
  if (glob) {
    const globPatterns = [];
    for (const rawPattern of glob.split(/\s+/)) {
      if (rawPattern.includes('{') && rawPattern.includes('}')) {
        globPatterns.push(rawPattern);
      } else {
        globPatterns.push(...rawPattern.split(',').filter(Boolean));
      }
    }
    for (const globPattern of globPatterns.filter(Boolean)) {
      args.push('--glob', globPattern);
    }
  }
  const results = await ripGrep(args, absolutePath);
  if (output_mode === 'content') {
    const { items, appliedLimit } = applyHeadLimit(results, headLimit, offset);
    const finalLines = items.map(line => {
      const colonIndex = line.indexOf(':');
      if (colonIndex > 0) {
        const filePath = line.substring(0, colonIndex);
        const rest = line.substring(colonIndex);
        return toRelativePath(filePath) + rest;
      }
      return line;
    });
    return {
      mode: 'content',
      numFiles: 0,
      filenames: [],
      content: finalLines.join('\n'),
      numLines: finalLines.length,
      ...(appliedLimit !== undefined && { appliedLimit }),
      ...(offset > 0 && { appliedOffset: offset }),
    };
  }
  if (output_mode === 'count') {
    const { items, appliedLimit } = applyHeadLimit(results, headLimit, offset);
    const finalCountLines = items.map(line => {
      const colonIndex = line.lastIndexOf(':');
      if (colonIndex > 0) {
        const filePath = line.substring(0, colonIndex);
        const count = line.substring(colonIndex);
        return toRelativePath(filePath) + count;
      }
      return line;
    });
    let totalMatches = 0;
    let fileCount = 0;
    for (const line of finalCountLines) {
      const colonIndex = line.lastIndexOf(':');
      if (colonIndex > 0) {
        const count = parseInt(line.substring(colonIndex + 1), 10);
        if (!Number.isNaN(count)) {
          totalMatches += count;
          fileCount += 1;
        }
      }
    }
    return {
      mode: 'count',
      numFiles: fileCount,
      filenames: [],
      content: finalCountLines.join('\n'),
      numMatches: totalMatches,
      ...(appliedLimit !== undefined && { appliedLimit }),
      ...(offset > 0 && { appliedOffset: offset }),
    };
  }
  const { items: limitedResults, appliedLimit } = applyHeadLimit(results, headLimit, offset);
  const filenames = limitedResults.map(toRelativePath);
  return {
    mode: 'files_with_matches',
    numFiles: filenames.length,
    filenames,
    ...(appliedLimit !== undefined && { appliedLimit }),
    ...(offset > 0 && { appliedOffset: offset }),
  };
}

function grepToolResult(output, toolUseID) {
  const {
    mode = 'files_with_matches',
    numFiles,
    filenames,
    content,
    numMatches,
    appliedLimit,
    appliedOffset,
  } = output;
  if (mode === 'content') {
    const limitInfo = formatLimitInfo(appliedLimit, appliedOffset);
    const resultContent = content || 'No matches found';
    return {
      tool_use_id: toolUseID,
      type: 'tool_result',
      content: limitInfo
        ? `${resultContent}\n\n[Showing results with pagination = ${limitInfo}]`
        : resultContent,
    };
  }
  if (mode === 'count') {
    const limitInfo = formatLimitInfo(appliedLimit, appliedOffset);
    const rawContent = content || 'No matches found';
    const matches = numMatches ?? 0;
    const files = numFiles ?? 0;
    const summary = `\n\nFound ${matches} total ${matches === 1 ? 'occurrence' : 'occurrences'} across ${files} ${files === 1 ? 'file' : 'files'}.${limitInfo ? ` with pagination = ${limitInfo}` : ''}`;
    return {
      tool_use_id: toolUseID,
      type: 'tool_result',
      content: rawContent + summary,
    };
  }
  const limitInfo = formatLimitInfo(appliedLimit, appliedOffset);
  if (numFiles === 0) {
    return {
      tool_use_id: toolUseID,
      type: 'tool_result',
      content: 'No files found',
    };
  }
  return {
    tool_use_id: toolUseID,
    type: 'tool_result',
    content: `Found ${numFiles} ${plural(numFiles, 'file')}${limitInfo ? ` ${limitInfo}` : ''}\n${filenames.join('\n')}`,
  };
}

async function main() {
  const root = await mkdtemp(path.join(tmpdir(), 'claude-search-harness-'));
  await mkdir(path.join(root, 'src'), { recursive: true });
  await mkdir(path.join(root, '.git'), { recursive: true });
  await writeFile(path.join(root, 'src', 'one.rs'), 'needle\n');
  await writeFile(path.join(root, 'src', 'two.py'), 'needle again\nsecond needle\n');
  await writeFile(path.join(root, '.hidden'), 'hidden needle\n');
  await writeFile(path.join(root, '.git', 'config'), 'needle in git\n');
  await writeFile(path.join(root, 'src', 'long.txt'), `${'x'.repeat(600)} needle\n`);
  process.chdir(root);

  const cases = [
    ['glob_rs', 'Glob', { pattern: '*.rs' }],
    ['grep_files', 'Grep', { pattern: 'needle' }],
    ['grep_content', 'Grep', { pattern: 'needle', output_mode: 'content' }],
    ['grep_count', 'Grep', { pattern: 'needle', output_mode: 'count' }],
    ['grep_glob_type', 'Grep', { pattern: 'needle', glob: '*.rs,*.py', type: 'rust' }],
    ['grep_paged', 'Grep', { pattern: 'needle', output_mode: 'content', head_limit: 2, offset: 1 }],
  ];

  const outputs = {};
  for (const [name, tool, input] of cases) {
    const data = tool === 'Glob' ? await globToolCall(input) : await grepToolCall(input);
    const result = tool === 'Glob' ? globToolResult(data, name) : grepToolResult(data, name);
    outputs[name] = { input, data, result };
  }
  console.log(JSON.stringify(outputs, null, 2));
  await rm(root, { recursive: true, force: true });
}

main().catch(error => {
  console.error(error);
  process.exit(1);
});
