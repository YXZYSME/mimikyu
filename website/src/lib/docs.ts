import fs from 'node:fs';
import path from 'node:path';
import { Marked } from 'marked';
import release from '../data/release.json';

export interface DocMeta {
  slug: string;
  filename: string;
  title: string;
  badge: string;
  accent: 'gold' | 'mint' | 'violet' | 'coral';
  summary: string;
}

export interface DocHeading {
  depth: number;
  text: string;
  id: string;
}

export interface RenderedDoc extends DocMeta {
  html: string;
  headings: DocHeading[];
  readingMinutes: number;
  githubUrl: string;
}

const GITHUB_REPO = 'https://github.com/YXZYSME/mimikyu';

export const DOCS_MANIFEST: DocMeta[] = [
  {
    slug: 'architecture',
    filename: 'ARCHITECTURE.md',
    title: 'Architecture & Scaling Invariants',
    badge: 'RUNTIME',
    accent: 'mint',
    summary:
      'Workspace module graph, DriverSession CDP/BiDi boundary, ChokepointInput serialization, and the five non-negotiable invariants.',
  },
  {
    slug: 'security',
    filename: 'SECURITY.md',
    title: 'Security Policy & R3 Boundary',
    badge: 'R3 POLICY',
    accent: 'coral',
    summary:
      'Private vulnerability disclosure, Unix-socket & ProxyRelay credential isolation, sandboxed eval, and strict R3 compliance boundaries.',
  },
  {
    slug: 'release',
    filename: 'RELEASE.md',
    title: 'Release & Version Control',
    badge: 'SEMVER',
    accent: 'gold',
    summary:
      'SemVer 2.0.0 workspace sync, trunk-based branching, SLSA v1 provenance attestations, and install.sh --update lifecycle.',
  },
  {
    slug: 'changelog',
    filename: 'CHANGELOG.md',
    title: 'Changelog',
    badge: 'HISTORY',
    accent: 'violet',
    summary:
      `Keep-a-Changelog release history covering dual-engine drivers, two-tier isolation, @eN snapshots, and ${release.mcp.total} MCP tools.`,
  },
  {
    slug: 'contributing',
    filename: 'CONTRIBUTING.md',
    title: 'Contributing Guide',
    badge: 'COMMUNITY',
    accent: 'mint',
    summary:
      'Local Rust 1.88+ setup, Conventional Commits, narrow vertical PR rules, and the pre-PR Clippy + Rust + E2E verification gauntlet.',
  },
];

const DOC_SLUG_BY_FILENAME = new Map(
  DOCS_MANIFEST.map((d) => [d.filename.toLowerCase(), d.slug])
);

function slugifyHeading(raw: string): string {
  return raw
    .toLowerCase()
    .replace(/<[^>]+>/g, '')
    .replace(/[`*_~]/g, '')
    .replace(/[^a-z0-9\s-]/g, '')
    .trim()
    .replace(/\s+/g, '-');
}

function repoRoot(): string {
  // website/ is one directory below the repository root
  return path.resolve(process.cwd(), '..');
}

function withBase(base: string, subpath: string): string {
  const cleanBase = base.endsWith('/') ? base : `${base}/`;
  const cleanPath = subpath.startsWith('/') ? subpath.slice(1) : subpath;
  return `${cleanBase}${cleanPath}`;
}

export function loadDoc(slug: string, basePath = '/'): RenderedDoc {
  const meta = DOCS_MANIFEST.find((d) => d.slug === slug);
  if (!meta) {
    throw new Error(`Unknown doc slug: ${slug}`);
  }

  const filePath = path.join(repoRoot(), meta.filename);
  const rawMarkdown = fs.readFileSync(filePath, 'utf-8');

  const headings: DocHeading[] = [];
  const usedIds = new Map<string, number>();

  const marked = new Marked({
    gfm: true,
    breaks: false,
  });

  marked.use({
    renderer: {
      heading({ tokens, depth }) {
        const innerHtml = this.parser.parseInline(tokens);
        const plainText = innerHtml.replace(/<[^>]+>/g, '').trim();
        const baseId = slugifyHeading(plainText) || `section-${headings.length + 1}`;
        const count = usedIds.get(baseId) ?? 0;
        usedIds.set(baseId, count + 1);
        const id = count === 0 ? baseId : `${baseId}-${count + 1}`;

        if (depth === 2 || depth === 3) {
          headings.push({ depth, text: plainText, id });
        }
        return `<h${depth} id="${id}">${innerHtml}</h${depth}>\n`;
      },
      blockquote({ tokens }) {
        const body = this.parser.parse(tokens);
        const alertMatch = body.match(
          /^<p>\[!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)\](?:<br\s*\/?>|\s+)([\s\S]*?)<\/p>([\s\S]*)$/i
        );
        if (alertMatch) {
          const kind = alertMatch[1].toLowerCase();
          const firstPara = alertMatch[2].trim();
          const rest = alertMatch[3] || '';
          return `<div class="gh-alert gh-alert-${kind}"><div class="gh-alert-title">${kind}</div><p>${firstPara}</p>${rest}</div>\n`;
        }
        return `<blockquote>${body}</blockquote>\n`;
      },
      link({ href, title, tokens }) {
        const text = this.parser.parseInline(tokens);
        let resolvedHref = href || '#';
        let external = false;

        if (/^https?:\/\//i.test(resolvedHref)) {
          external = true;
        } else if (!resolvedHref.startsWith('#') && !resolvedHref.startsWith('mailto:')) {
          const [pathPart, hashPart] = resolvedHref.split('#');
          const cleanFile = pathPart.replace(/^\.\//, '');
          const mappedSlug = DOC_SLUG_BY_FILENAME.get(cleanFile.toLowerCase());
          if (mappedSlug) {
            resolvedHref = withBase(basePath, `docs/${mappedSlug}`) + (hashPart ? `#${hashPart}` : '');
          } else {
            resolvedHref = `${GITHUB_REPO}/blob/main/${cleanFile}${hashPart ? `#${hashPart}` : ''}`;
            external = true;
          }
        }

        const titleAttr = title ? ` title="${title.replace(/"/g, '&quot;')}"` : '';
        const targetAttr = external ? ' target="_blank" rel="noopener noreferrer"' : '';
        return `<a href="${resolvedHref}"${titleAttr}${targetAttr}>${text}</a>`;
      },
    },
  });

  const html = marked.parse(rawMarkdown) as string;
  const wordCount = rawMarkdown.trim().split(/\s+/).length;
  const readingMinutes = Math.max(1, Math.round(wordCount / 220));

  return {
    ...meta,
    html,
    headings,
    readingMinutes,
    githubUrl: `${GITHUB_REPO}/blob/main/${meta.filename}`,
  };
}

export function loadAllDocs(basePath = '/'): RenderedDoc[] {
  return DOCS_MANIFEST.map((d) => loadDoc(d.slug, basePath));
}
