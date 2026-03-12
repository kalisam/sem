import { describe, it, expect } from 'vitest';
import { matchEntities, defaultSimilarity } from '../src/model/identity.js';
import { contentHash } from '../src/utils/hash.js';
import type { SemanticEntity } from '../src/model/entity.js';

function makeEntity(overrides: Partial<SemanticEntity> & { id: string; name: string; content: string }): SemanticEntity {
  const { name, content } = overrides;
  // Derive bodyContent: everything after the entity name (simulates tree-sitter extraction)
  const nameIdx = content.indexOf(name);
  const bodyContent = nameIdx >= 0 ? content.slice(nameIdx + name.length).trim() : undefined;

  return {
    filePath: 'src/components/UserCard.tsx',
    entityType: 'function',
    startLine: 1,
    endLine: 20,
    contentHash: contentHash(overrides.content),
    bodyContent,
    ...overrides,
  };
}

describe('rename detection', () => {
  it('detects rename when function name changed but body is mostly the same', () => {
    // React component renamed UserCard → ProfileCard, minor body tweaks
    const before = [makeEntity({
      id: 'src/components/UserCard.tsx::function::UserCard',
      name: 'UserCard',
      content: `const UserCard = ({ user }: { user: User }) => {
  const [isExpanded, setIsExpanded] = useState(false);
  const roles = useUserRoles(user.id);

  return (
    <div className="card p-4 rounded-lg shadow">
      <Avatar src={user.avatar} size="md" />
      <h3>{user.name}</h3>
      <span className="text-muted">{user.email}</span>
      {isExpanded && (
        <div className="mt-2">
          {roles.map(r => <Badge key={r}>{r}</Badge>)}
        </div>
      )}
      <button onClick={() => setIsExpanded(!isExpanded)}>
        {isExpanded ? "Less" : "More"}
      </button>
    </div>
  );
};`,
      startLine: 5,
      endLine: 25,
    })];

    const after = [makeEntity({
      id: 'src/components/UserCard.tsx::function::ProfileCard',
      name: 'ProfileCard',
      content: `const ProfileCard = ({ user }: { user: User }) => {
  const [isExpanded, setIsExpanded] = useState(false);
  const roles = useUserRoles(user.id);
  const initials = getInitials(user.name);

  return (
    <div className="card p-4 rounded-lg shadow-md border">
      <Avatar src={user.avatar} fallback={initials} size="lg" />
      <h3>{user.name}</h3>
      <span className="text-sm text-gray-500">{user.email}</span>
      {isExpanded && (
        <div className="mt-3 space-y-1">
          {roles.map(r => <Badge variant="outline" key={r}>{r}</Badge>)}
        </div>
      )}
      <button onClick={() => setIsExpanded(!isExpanded)}>
        {isExpanded ? "Show less" : "Show more"}
      </button>
    </div>
  );
};`,
      startLine: 5,
      endLine: 27,
    })];

    const result = matchEntities(before, after, 'src/components/UserCard.tsx', defaultSimilarity);
    expect(result.changes).toHaveLength(1);
    expect(result.changes[0].changeType).toBe('renamed');
    expect(result.changes[0].entityName).toBe('ProfileCard');
  });

  it('does NOT rename completely different functions that happen to share a signature', () => {
    // farewell and welcome are genuinely different functions — not a rename
    const before = [makeEntity({
      id: 'utils.ts::function::farewell',
      name: 'farewell',
      filePath: 'utils.ts',
      content: `function farewell(name: string): string {
  return \`Goodbye, \${name}!\`;
}`,
    })];

    const after = [makeEntity({
      id: 'utils.ts::function::welcome',
      name: 'welcome',
      filePath: 'utils.ts',
      content: `function welcome(name: string): string {
  return \`Welcome, \${name}!\`;
}`,
    })];

    const result = matchEntities(before, after, 'utils.ts', defaultSimilarity);
    expect(result.changes).toHaveLength(2);
    const types = result.changes.map(c => c.changeType).sort();
    expect(types).toEqual(['added', 'deleted']);
  });

  it('detects rename of arrow function with significant body changes', () => {
    // formatUserName → buildDisplayName, return expression changed
    const before = [makeEntity({
      id: 'service.ts::function::formatUserName',
      name: 'formatUserName',
      filePath: 'service.ts',
      content: `const formatUserName = (first: string, last: string): string => {
  return \`\${last}, \${first}\`;
};`,
      startLine: 95,
      endLine: 97,
    })];

    const after = [makeEntity({
      id: 'service.ts::function::buildDisplayName',
      name: 'buildDisplayName',
      filePath: 'service.ts',
      content: `const buildDisplayName = (first: string, last: string): string => {
  return \`\${first.charAt(0)}. \${last}\`;
};`,
      startLine: 119,
      endLine: 121,
    })];

    const result = matchEntities(before, after, 'service.ts', defaultSimilarity);
    expect(result.changes).toHaveLength(1);
    expect(result.changes[0].changeType).toBe('renamed');
    expect(result.changes[0].entityName).toBe('buildDisplayName');
  });

  it('detects rename of hook with added logic', () => {
    // useAuth → useAuthentication, added token refresh logic
    const before = [makeEntity({
      id: 'hooks.ts::function::useAuth',
      name: 'useAuth',
      filePath: 'hooks.ts',
      content: `const useAuth = () => {
  const [user, setUser] = useState<User | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    const session = getSession();
    setUser(session?.user ?? null);
    setLoading(false);
  }, []);

  const login = async (email: string, password: string) => {
    const res = await api.post('/auth/login', { email, password });
    setUser(res.data.user);
  };

  const logout = async () => {
    await api.post('/auth/logout');
    setUser(null);
  };

  return { user, loading, login, logout };
};`,
      startLine: 10,
      endLine: 30,
    })];

    const after = [makeEntity({
      id: 'hooks.ts::function::useAuthentication',
      name: 'useAuthentication',
      filePath: 'hooks.ts',
      content: `const useAuthentication = () => {
  const [user, setUser] = useState<User | null>(null);
  const [loading, setLoading] = useState(true);
  const [token, setToken] = useState<string | null>(null);

  useEffect(() => {
    const session = getSession();
    setUser(session?.user ?? null);
    setToken(session?.token ?? null);
    setLoading(false);
  }, []);

  const login = async (email: string, password: string) => {
    const res = await api.post('/auth/login', { email, password });
    setUser(res.data.user);
    setToken(res.data.token);
  };

  const logout = async () => {
    await api.post('/auth/logout');
    setUser(null);
    setToken(null);
  };

  return { user, loading, token, login, logout };
};`,
      startLine: 10,
      endLine: 34,
    })];

    const result = matchEntities(before, after, 'hooks.ts', defaultSimilarity);
    expect(result.changes).toHaveLength(1);
    expect(result.changes[0].changeType).toBe('renamed');
    expect(result.changes[0].entityName).toBe('useAuthentication');
  });

  it('does NOT rename functions with different types even if body matches', () => {
    const before = [makeEntity({
      id: 'utils.ts::function::parse',
      name: 'parse',
      filePath: 'utils.ts',
      entityType: 'function',
      content: `function parse(input: string) { return JSON.parse(input); }`,
    })];

    const after = [makeEntity({
      id: 'utils.ts::class::parse',
      name: 'Parser',
      filePath: 'utils.ts',
      entityType: 'class',
      content: `class Parser { parse(input: string) { return JSON.parse(input); } }`,
    })];

    const result = matchEntities(before, after, 'utils.ts', defaultSimilarity);
    const types = result.changes.map(c => c.changeType).sort();
    expect(types).toEqual(['added', 'deleted']);
  });

  it('detects rename among multiple functions — best match wins', () => {
    // fetchUsers renamed to getUsers, fetchPosts stays deleted, createPost is added
    const before = [
      makeEntity({
        id: 'api.ts::function::fetchUsers',
        name: 'fetchUsers',
        filePath: 'api.ts',
        content: `const fetchUsers = async (page: number, limit: number) => {
  const res = await api.get('/users', { params: { page, limit } });
  return res.data.users;
};`,
        startLine: 10,
        endLine: 13,
      }),
      makeEntity({
        id: 'api.ts::function::fetchPosts',
        name: 'fetchPosts',
        filePath: 'api.ts',
        content: `const fetchPosts = async (userId: string) => {
  const res = await api.get(\`/users/\${userId}/posts\`);
  return res.data.posts;
};`,
        startLine: 20,
        endLine: 23,
      }),
    ];

    const after = [
      makeEntity({
        id: 'api.ts::function::getUsers',
        name: 'getUsers',
        filePath: 'api.ts',
        content: `const getUsers = async (page: number, limit: number) => {
  const res = await api.get('/users', { params: { page, limit } });
  return res.data.users;
};`,
        startLine: 10,
        endLine: 13,
      }),
      makeEntity({
        id: 'api.ts::function::createPost',
        name: 'createPost',
        filePath: 'api.ts',
        content: `const createPost = async (userId: string, title: string, body: string) => {
  const res = await api.post(\`/users/\${userId}/posts\`, { title, body });
  return res.data.post;
};`,
        startLine: 20,
        endLine: 23,
      }),
    ];

    const result = matchEntities(before, after, 'api.ts', defaultSimilarity);
    const changes = Object.fromEntries(result.changes.map(c => [c.entityName, c.changeType]));
    expect(changes['getUsers']).toBe('renamed');       // fetchUsers → getUsers
    expect(changes['fetchPosts']).toBe('deleted');      // genuinely removed
    expect(changes['createPost']).toBe('added');        // genuinely new
  });
});
