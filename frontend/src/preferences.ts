export type PreferenceName = 'theme' | 'nav-expanded' | 'timezone' | 'ai-model' | 'ai-timeout' | 'ai-check-in' | 'histogram-visible';

const legacyKeys: Partial<Record<PreferenceName, string>> = {
  theme: 'bm9ydGhzdGFyLXRoZW1l',
  'nav-expanded': 'bm9ydGhzdGFyLW5hdi1leHBhbmRlZA==',
  timezone: 'bm9ydGhzdGFyLXRpbWV6b25l',
  'ai-model': 'bm9ydGhzdGFyLWFpLW1vZGVs',
};

function currentKey(name: PreferenceName) {
  return `investigenie-${name}`;
}

function legacyKey(name: PreferenceName): string | null {
  const encoded = legacyKeys[name];
  return encoded ? window.atob(encoded) : null;
}

export function getPreference(name: PreferenceName): string | null {
  try {
    const key = currentKey(name);
    const legacyName = legacyKey(name);
    const current = window.localStorage.getItem(key);
    const legacy = legacyName === null ? null : window.localStorage.getItem(legacyName);
    if (current !== null) {
      if (legacy !== null) window.localStorage.removeItem(legacyName!);
      return current;
    }
    if (legacy !== null) {
      window.localStorage.setItem(key, legacy);
      window.localStorage.removeItem(legacyName!);
    }
    return legacy;
  } catch {
    return null;
  }
}

export function setPreference(name: PreferenceName, value: string) {
  try {
    window.localStorage.setItem(currentKey(name), value);
    const legacyName = legacyKey(name);
    if (legacyName !== null) window.localStorage.removeItem(legacyName);
  } catch {}
}

export function clearPreference(name: PreferenceName) {
  try {
    window.localStorage.removeItem(currentKey(name));
    const legacyName = legacyKey(name);
    if (legacyName !== null) window.localStorage.removeItem(legacyName);
  } catch {}
}