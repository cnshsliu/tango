import type { McpServerSpec } from './mcp.js';

export interface CatalogEntry {
  id: string;
  name: string;
  description: string;
  /** what the user must do before it works, e.g. set an environment variable */
  auth: string;
  spec: McpServerSpec;
  docs: string;
}

/**
 * Well-known remote MCP servers, reachable with a URL and (sometimes) a token — no install.
 * Header values use `${VAR}`: the token is read from the environment and never sits in a config file.
 * Endpoints were checked live on 2026-11-25 (each lists tools, or answers 401 without a token).
 */
export const MCP_CATALOG: CatalogEntry[] = [
  {
    id: 'context7',
    name: 'Context7',
    description: 'Up-to-date library documentation and version-specific code examples',
    auth: 'none',
    spec: { url: 'https://mcp.context7.com/mcp' },
    docs: 'https://github.com/upstash/context7'
  },
  {
    id: 'deepwiki',
    name: 'DeepWiki',
    description: 'Ask questions about open-source repositories on GitHub',
    auth: 'none',
    spec: { url: 'https://mcp.deepwiki.com/mcp' },
    docs: 'https://deepwiki.com'
  },
  {
    id: 'github',
    name: 'GitHub',
    description: 'Repos, issues, pull requests, Actions, code security (official remote server)',
    auth: 'a GitHub PAT in the environment: export GITHUB_PAT=…',
    spec: {
      url: 'https://api.githubcopilot.com/mcp/',
      headers: { Authorization: 'Bearer ${GITHUB_PAT}' }
    },
    docs: 'https://github.com/github/github-mcp-server'
  },
  {
    id: 'sentry',
    name: 'Sentry',
    description: 'Issues, events and stack traces of your Sentry projects',
    auth: 'a Sentry access token in the environment: export SENTRY_ACCESS_TOKEN=…',
    spec: {
      url: 'https://mcp.sentry.dev/mcp',
      headers: { Authorization: 'Bearer ${SENTRY_ACCESS_TOKEN}' }
    },
    docs: 'https://docs.sentry.io/product/sentry-mcp/'
  }
];
