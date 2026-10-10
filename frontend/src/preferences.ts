export type PreferenceName = 'theme' | 'nav-expanded' | 'timezone' | 'ai-model';

const legacyKeys: Record<PreferenceName, string> = {
  theme: 'bm9ydGhzdGFyLXRoZW1l',
  'nav-expanded': 'bm9ydGhzdGFyLW5hdi1leHBhbmRlZA==',
  timezone: 'bm9ydGhzdGFyLXRpbWV6b25l',
  'ai-model': 'bm9ydGhzdGFyLWFpLW1vZGVs',
};

function currentKey(name: PreferenceName) {
  return `investigenie-${name}`;
}

export function getPreference(name: PreferenceName): string | null {
  try {
    const key = currentKey(name);
    const legacyKey = window.atob(legacyKeys[name]);
    const current = window.localStorage.getItem(key);
    const legacy = window.localStorage.getItem(legacyKey);
    if (current !== null) {
      if (legacy !== null) window.localStorage.removeItem(legacyKey);
      return current;
    }
    if (legacy !== null) {
      window.localStorage.setItem(key, legacy);
      window.localStorage.removeItem(legacyKey);
    }
    return legacy;
  } catch {
    return null;
  }
}

export function setPreference(name: PreferenceName, value: string) {
  try {
    window.localStorage.setItem(currentKey(name), value);
    window.localStorage.removeItem(window.atob(legacyKeys[name]));
  } catch {}
}

export function clearPreference(name: PreferenceName) {
  try {
    window.localStorage.removeItem(currentKey(name));
    window.localStorage.removeItem(window.atob(legacyKeys[name]));
  } catch {}
}