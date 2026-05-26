import { readFileSync } from 'node:fs';

const rustFileTools = readFileSync('codex-rs/core/src/tools/handlers/simple_file_tools_spec.rs', 'utf8');
const rustSearchTools = readFileSync('codex-rs/core/src/tools/handlers/simple_search_tools_spec.rs', 'utf8');
const claudeRoot = '/Users/yuguorui/Code/opencode/claude-code/src';
const claudeBash = readFileSync(`${claudeRoot}/tools/BashTool/BashTool.tsx`, 'utf8');
const claudePrompt = readFileSync(`${claudeRoot}/tools/BashTool/prompt.ts`, 'utf8');
const claudeToolName = readFileSync(`${claudeRoot}/tools/BashTool/toolName.ts`, 'utf8');
const claudeFileReadPrompt = readFileSync(`${claudeRoot}/tools/FileReadTool/prompt.ts`, 'utf8');
const claudeFileWritePrompt = readFileSync(`${claudeRoot}/tools/FileWriteTool/prompt.ts`, 'utf8');
const claudeFileEditConstants = readFileSync(`${claudeRoot}/tools/FileEditTool/constants.ts`, 'utf8');
const claudeGlobPrompt = readFileSync(`${claudeRoot}/tools/GlobTool/prompt.ts`, 'utf8');
const claudeGrepPrompt = readFileSync(`${claudeRoot}/tools/GrepTool/prompt.ts`, 'utf8');
const claudeAgentConstants = readFileSync(`${claudeRoot}/tools/AgentTool/constants.ts`, 'utf8');
const claudeFileEditPrompt = readFileSync(`${claudeRoot}/tools/FileEditTool/prompt.ts`, 'utf8');
const claudeFileWriteTool = readFileSync(`${claudeRoot}/tools/FileWriteTool/FileWriteTool.ts`, 'utf8');
const claudeFileReadTool = readFileSync(`${claudeRoot}/tools/FileReadTool/FileReadTool.ts`, 'utf8');
const claudeTimeouts = readFileSync(`${claudeRoot}/utils/timeouts.ts`, 'utf8');
const simplePromptStart = claudePrompt.indexOf('export function getSimplePrompt(): string {');
if (simplePromptStart === -1) fail('failed to find getSimplePrompt');
const simplePrompt = claudePrompt.slice(simplePromptStart);

function fail(message) {
  console.error(message);
  process.exit(1);
}

function assertEqual(name, actual, expected) {
  if (actual !== expected) {
    console.error(`Mismatch: ${name}`);
    console.error('--- actual ---');
    console.error(actual);
    console.error('--- expected ---');
    console.error(expected);
    process.exit(1);
  }
}

function requireMatch(source, regex, name) {
  const match = source.match(regex);
  if (!match) fail(`failed to extract ${name}`);
  return match;
}

function rustRawStringAfter(source, marker) {
  const start = source.indexOf(marker);
  if (start === -1) fail(`missing Rust marker ${marker}`);
  const rawStart = source.indexOf('r#"', start);
  if (rawStart === -1) fail(`missing raw string after ${marker}`);
  const contentStart = rawStart + 3;
  const rawEnd = source.indexOf('"#', contentStart);
  if (rawEnd === -1) fail(`missing raw string terminator after ${marker}`);
  return source.slice(contentStart, rawEnd);
}

function tsStringValue(source, exportName) {
  return requireMatch(
    source,
    new RegExp(`export const ${exportName} = '([^']+)'`),
    exportName,
  )[1];
}

function timeoutValue(exportName) {
  const constantName =
    exportName === 'getDefaultBashTimeoutMs' ? 'DEFAULT_TIMEOUT_MS' : 'MAX_TIMEOUT_MS';
  const functionBody = requireMatch(
    claudeTimeouts,
    new RegExp(`export function ${exportName}\\([^)]*\\): number \\{[\\s\\S]*?return (?:Math\\.max\\()?(${constantName})`),
    exportName,
  )[1];
  const constantValue = requireMatch(
    claudeTimeouts,
    new RegExp(`const ${functionBody} = ([0-9_]+)`),
    functionBody,
  )[1];
  return Number(constantValue.replaceAll('_', ''));
}

const names = {
  BASH_TOOL_NAME: tsStringValue(claudeToolName, 'BASH_TOOL_NAME'),
  FILE_READ_TOOL_NAME: tsStringValue(claudeFileReadPrompt, 'FILE_READ_TOOL_NAME'),
  FILE_WRITE_TOOL_NAME: tsStringValue(claudeFileWritePrompt, 'FILE_WRITE_TOOL_NAME'),
  FILE_EDIT_TOOL_NAME: tsStringValue(claudeFileEditConstants, 'FILE_EDIT_TOOL_NAME'),
  GLOB_TOOL_NAME: tsStringValue(claudeGlobPrompt, 'GLOB_TOOL_NAME'),
  GREP_TOOL_NAME: tsStringValue(claudeGrepPrompt, 'GREP_TOOL_NAME'),
  AGENT_TOOL_NAME: tsStringValue(claudeAgentConstants, 'AGENT_TOOL_NAME'),
};
const defaultTimeoutMs = timeoutValue('getDefaultBashTimeoutMs');
const maxTimeoutMs = timeoutValue('getMaxBashTimeoutMs');

function interpolateClaudeTemplate(text) {
  return text
    .replaceAll('${BASH_TOOL_NAME}', names.BASH_TOOL_NAME)
    .replaceAll('${FILE_READ_TOOL_NAME}', names.FILE_READ_TOOL_NAME)
    .replaceAll('${FILE_WRITE_TOOL_NAME}', names.FILE_WRITE_TOOL_NAME)
    .replaceAll('${FILE_EDIT_TOOL_NAME}', names.FILE_EDIT_TOOL_NAME)
    .replaceAll('${GLOB_TOOL_NAME}', names.GLOB_TOOL_NAME)
    .replaceAll('${GREP_TOOL_NAME}', names.GREP_TOOL_NAME)
    .replaceAll('${AGENT_TOOL_NAME}', names.AGENT_TOOL_NAME)
    .replaceAll('${getMaxTimeoutMs()}', String(maxTimeoutMs))
    .replaceAll('${getDefaultTimeoutMs()}', String(defaultTimeoutMs))
    .replaceAll('${getMaxTimeoutMs() / 60000}', String(maxTimeoutMs / 60000))
    .replaceAll('${getDefaultTimeoutMs() / 60000}', String(defaultTimeoutMs / 60000));
}

function decodeTsString(text) {
  return text
    .replaceAll('\\\\', '\\')
    .replaceAll('\\u2014', '—')
    .replaceAll("\\'", "'")
    .replaceAll('\\`', '`');
}

function extractTemplateAfter(source, marker) {
  const markerStart = source.indexOf(marker);
  if (markerStart === -1) fail(`failed to find template marker ${marker}`);
  const returnStart = source.indexOf('return `', markerStart);
  if (returnStart === -1) fail(`failed to find template return after ${marker}`);
  const contentStart = returnStart + 'return `'.length;
  for (let index = contentStart; index < source.length; index += 1) {
    if (source[index] === '`' && source[index - 1] !== '\\') {
      return source.slice(contentStart, index);
    }
  }
  fail(`failed to find template end after ${marker}`);
}

function extractStringLiterals(arrayBody, name) {
  const values = [];
  const literalRegex = /(?:`((?:\\.|[^`])*)`|'((?:\\.|[^'])*)'|"((?:\\.|[^"])*)")/g;
  for (const match of arrayBody.matchAll(literalRegex)) {
    values.push(interpolateClaudeTemplate(decodeTsString(match[1] ?? match[2] ?? match[3])));
  }
  if (values.length === 0) fail(`failed to extract string literals from ${name}`);
  return values;
}

function arrayBodyAfter(name) {
  const start = simplePrompt.indexOf(`const ${name} = [`);
  if (start === -1) fail(`failed to find array ${name}`);
  const bodyStart = simplePrompt.indexOf('[', start) + 1;
  const end = simplePrompt.indexOf('\n  ]', bodyStart);
  if (end === -1) fail(`failed to find end of array ${name}`);
  return simplePrompt.slice(bodyStart, end);
}

function namedArray(name) {
  return extractStringLiterals(arrayBodyAfter(name), name);
}

function prependBullets(items) {
  return items.flatMap(item =>
    Array.isArray(item)
      ? item.map(subitem => `  - ${subitem}`)
      : [` - ${item}`],
  );
}

function requireRustFunctionStrict(source, functionName) {
  const start = source.indexOf(`pub(crate) fn ${functionName}`);
  if (start === -1) fail(`missing Rust function ${functionName}`);
  const nextFunction = source.indexOf('\npub(crate) fn ', start + 1);
  const end = nextFunction === -1 ? source.length : nextFunction;
  const body = source.slice(start, end);
  if (!body.includes('strict: true')) fail(`${functionName} is not strict`);
}

const descriptionParam = rustRawStringAfter(
  rustFileTools,
  'fn bash_description_parameter_description',
);
const claudeDescriptionRuntime = requireMatch(
  claudeBash,
  /description: z\.string\(\)\.optional\(\)\.describe\(`([\s\S]*?)`\),\n  run_in_background:/,
  'Claude Bash description parameter schema',
)[1].replaceAll('\\\\', '\\');
assertEqual('description parameter schema', descriptionParam, claudeDescriptionRuntime);

const propertyChecks = [
  ['command', /command: z\.string\(\)\.describe\('([^']*)'\)/],
  ['timeout', /timeout: semanticNumber\(z\.number\(\)\.optional\(\)\)\.describe\(`Optional timeout in milliseconds \(max \$\{getMaxTimeoutMs\(\)\}\)`\)/],
  ['run_in_background', /run_in_background: semanticBoolean\(z\.boolean\(\)\.optional\(\)\)\.describe\(`([^`]*)`\)/],
  ['dangerouslyDisableSandbox', /dangerouslyDisableSandbox: semanticBoolean\(z\.boolean\(\)\.optional\(\)\)\.describe\('([^']*)'\)/],
];
for (const [name, regex] of propertyChecks) {
  requireMatch(claudeBash, regex, `Claude schema property ${name}`);
}

const nonEmbeddedToolPreferenceBlock = requireMatch(
  simplePrompt,
  /\.\.\.\(embedded\n      \? \[\]\n      : \[\n([\s\S]*?)\n        \]\),/,
  'non-embedded tool preference items',
)[1];
const commonToolPreferenceBlock = requireMatch(
  simplePrompt,
  /\]\),\n([\s\S]*?)\n  \]\n\n  const avoidCommands/,
  'common tool preference items',
)[1];
const toolPreferenceItems = [
  ...extractStringLiterals(nonEmbeddedToolPreferenceBlock, 'non-embedded tool preferences'),
  ...extractStringLiterals(commonToolPreferenceBlock, 'common tool preferences'),
];

const avoidCommands = requireMatch(
  simplePrompt,
  /const avoidCommands = embedded\n    \? '[^']*'\n    : '([^']+)'/,
  'non-embedded avoid commands',
)[1];
const backgroundNote = requireMatch(
  claudePrompt,
  /return "([^"]+run_in_background[^"]+)"/,
  'background usage note',
)[1];
const sleepBody = arrayBodyAfter('sleepSubitems');
const firstMonitorBranch = sleepBody.indexOf("    ...(feature('MONITOR_TOOL')");
const middleSleepStart = sleepBody.indexOf("    'If your command", firstMonitorBranch);
const secondMonitorBranch = sleepBody.indexOf("    ...(feature('MONITOR_TOOL')", middleSleepStart);
if (firstMonitorBranch === -1 || middleSleepStart === -1 || secondMonitorBranch === -1) {
  fail('failed to locate sleepSubitems branches');
}
const sleepSubitems = [
  ...extractStringLiterals(
    sleepBody.slice(0, firstMonitorBranch),
    'sleep subitems before first monitor branch',
  ),
  ...extractStringLiterals(
    sleepBody.slice(middleSleepStart, secondMonitorBranch),
    'sleep subitems between monitor branches',
  ),
  ...extractStringLiterals(
    requireMatch(
      sleepBody.slice(secondMonitorBranch),
      /\: \[\n([\s\S]*?)\n        \]\),/,
      'sleep subitems second non-monitor branch',
    )[1],
    'sleep subitems second non-monitor branch',
  ),
];

const instructionItems = [
  ...extractStringLiterals(
    requireMatch(
      simplePrompt,
      /const instructionItems: Array<string \| string\[]> = \[\n([\s\S]*?)\n    `You may specify/,
      'instruction items before timeout',
    )[1],
    'instruction items before timeout',
  ),
  interpolateClaudeTemplate(
    requireMatch(
      simplePrompt,
      /(`You may specify an optional timeout in milliseconds[^`]+`),/,
      'timeout instruction',
    )[1].slice(1, -1),
  ),
  backgroundNote,
  'When issuing multiple commands:',
  namedArray('multipleCommandsSubitems'),
  'For git commands:',
  namedArray('gitSubitems'),
  'Avoid unnecessary `sleep` commands:',
  sleepSubitems,
];

const leadingPromptItems = extractStringLiterals(
  requireMatch(
    simplePrompt,
    /return \[\n([\s\S]*?)\n    \.\.\.prependBullets\(toolPreferenceItems\),/,
    'getSimplePrompt leading items',
  )[1],
  'getSimplePrompt leading items',
);
leadingPromptItems[4] = leadingPromptItems[4].replace('${avoidCommands}', avoidCommands);
const whileLine = interpolateClaudeTemplate(
  requireMatch(
    simplePrompt,
    /(`While the \$\{BASH_TOOL_NAME\} tool can do similar things[^`]+`),/,
    'while Bash tool line',
  )[1].slice(1, -1),
);

const expectedPrompt = [
  ...leadingPromptItems,
  ...prependBullets(toolPreferenceItems),
  whileLine,
  '',
  '# Instructions',
  ...prependBullets(instructionItems),
].join('\n');

const rustPrompt = rustRawStringAfter(rustFileTools, 'fn bash_tool_description')
  .replaceAll('{BASH_MAX_TIMEOUT_MS}', String(maxTimeoutMs))
  .replaceAll('{max_timeout_minutes}', String(maxTimeoutMs / 60000))
  .replaceAll('{BASH_DEFAULT_TIMEOUT_MS}', String(defaultTimeoutMs))
  .replaceAll('{default_timeout_minutes}', String(defaultTimeoutMs / 60000));

assertEqual('Bash prompt rendered from Claude Code source', rustPrompt, expectedPrompt);

requireRustFunctionStrict(rustFileTools, 'create_bash_tool');

const globDescription = requireMatch(
  claudeGlobPrompt,
  /export const DESCRIPTION = `([\s\S]*?)`/,
  'Claude Glob description',
)[1];
assertEqual(
  'Glob description',
  rustRawStringAfter(rustSearchTools, 'fn glob_tool_description'),
  globDescription,
);
requireRustFunctionStrict(rustSearchTools, 'create_glob_tool');

const grepDescription = interpolateClaudeTemplate(decodeTsString(
  requireMatch(
    claudeGrepPrompt,
    /return `([\s\S]*?)`\n}/,
    'Claude Grep description',
  )[1],
));
assertEqual(
  'Grep description',
  rustRawStringAfter(rustSearchTools, 'fn grep_tool_description'),
  grepDescription,
);
requireRustFunctionStrict(rustSearchTools, 'create_grep_tool');

const editDescriptionTemplate = extractTemplateAfter(
  claudeFileEditPrompt,
  'function getDefaultEditDescription(): string',
);
const editPreRead = extractTemplateAfter(
  claudeFileEditPrompt,
  'function getPreReadInstruction(): string',
).replace(/^\\n/, '');
const editDescription = interpolateClaudeTemplate(decodeTsString(
  editDescriptionTemplate
    .replace('${getPreReadInstruction()}', `\n${decodeTsString(editPreRead)}`)
    .replace('${prefixFormat}', 'spaces + line number + arrow')
    .replace('${minimalUniquenessHint}', ''),
));
assertEqual(
  'Edit description',
  rustRawStringAfter(rustFileTools, 'fn edit_tool_description')
    .replaceAll('{pre_read_trailing_space}', ' '),
  editDescription,
);
requireRustFunctionStrict(rustFileTools, 'create_edit_tool');

const writeDescriptionTemplate = extractTemplateAfter(
  claudeFileWritePrompt,
  'export function getWriteToolDescription(): string',
);
const writePreRead = extractTemplateAfter(
  claudeFileWritePrompt,
  'function getPreReadInstruction(): string',
).replace(/^\\n/, '');
const writeDescription = interpolateClaudeTemplate(decodeTsString(
  writeDescriptionTemplate.replace('${getPreReadInstruction()}', `\n${decodeTsString(writePreRead)}`),
));
assertEqual(
  'Write description',
  rustRawStringAfter(rustFileTools, 'fn write_tool_description'),
  writeDescription,
);
requireRustFunctionStrict(rustFileTools, 'create_write_tool');

for (const [source, functionName] of [
  [rustFileTools, 'create_read_tool'],
  [rustFileTools, 'create_edit_tool'],
  [rustFileTools, 'create_write_tool'],
  [rustSearchTools, 'create_glob_tool'],
  [rustSearchTools, 'create_grep_tool'],
]) {
  requireRustFunctionStrict(source, functionName);
}

for (const [source, regex, name] of [
  [claudeFileReadTool, /file_path: z\.string\(\)\.describe\('The absolute path to the file to read'\)/, 'Read file_path schema'],
  [claudeFileEditConstants, /export const FILE_EDIT_TOOL_NAME = 'Edit'/, 'Edit tool name'],
  [claudeFileWriteTool, /file_path: z\s*\.\s*string\(\)\s*\.describe\(\s*'The absolute path to the file to write \(must be absolute, not relative\)'/m, 'Write file_path schema'],
]) {
  requireMatch(source, regex, name);
}

console.log('Claude-style simple tool schema/prompt checks passed.');
