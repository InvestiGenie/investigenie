import { beforeEach, describe, expect, it } from 'vitest';
import { clearPreference, getPreference, setPreference } from './preferences';

const legacyKeys = {
  theme: 'bm9ydGhzdGFyLXRoZW1l',
  'nav-expanded': 'bm9ydGhzdGFyLW5hdi1leHBhbmRlZA==',
  timezone: 'bm9ydGhzdGFyLXRpbWV6b25l',
  'ai-model': 'bm9ydGhzdGFyLWFpLW1vZGVs',
} as const;

describe('browser preferences', () => {
  beforeEach(() => window.localStorage.clear());

  it('migrates existing preferences to InvestiGenie keys and removes the previous key', () => {
    for (const [name, encodedLegacyKey] of Object.entries(legacyKeys)) {
      const legacyKey = window.atob(encodedLegacyKey);
      const value = `saved-${name}`;
      window.localStorage.setItem(legacyKey, value);
      expect(getPreference(name as keyof typeof legacyKeys)).toBe(value);
      expect(window.localStorage.getItem(`investigenie-${name}`)).toBe(value);
      expect(window.localStorage.getItem(legacyKey)).toBeNull();
    }
  });

  it('prefers a current preference and writes only the InvestiGenie key', () => {
    const legacyKey = window.atob(legacyKeys.theme);
    window.localStorage.setItem(legacyKey, 'light');
    window.localStorage.setItem('investigenie-theme', 'dark');
    expect(getPreference('theme')).toBe('dark');
    expect(window.localStorage.getItem(legacyKey)).toBeNull();
    setPreference('theme', 'light');
    expect(window.localStorage.getItem('investigenie-theme')).toBe('light');
    expect(window.localStorage.getItem(legacyKey)).toBeNull();
    clearPreference('theme');
    expect(window.localStorage.getItem('investigenie-theme')).toBeNull();
  });
});