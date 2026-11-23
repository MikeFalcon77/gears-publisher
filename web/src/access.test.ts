import { describe, expect, it } from 'vitest';
import { byStatus, providerLabel, who } from './access';
import { canControl, permittedActions } from './runview';
import type { UserEntry } from './types';

const user = (status: UserEntry['status'], name: string | null = null): UserEntry => ({
  subject_id: status,
  uid: `uid-${status}`,
  email: null,
  name,
  provider: 'github.com',
  note: 'hi',
  status,
  requested_at: '2026-11-23T00:00:00Z',
  decided_at: null,
  decided_by: null,
});

describe('access', () => {
  it('names providers and people', () => {
    expect(providerLabel('github.com')).toBe('GitHub');
    expect(providerLabel('google.com')).toBe('Google');
    expect(providerLabel(null)).toBe('—');
    expect(who(user('pending', 'Вася'))).toBe('Вася');
    expect(who(user('pending'))).toBe('uid-pending');
  });

  it('groups requests by status', () => {
    const groups = byStatus([user('pending'), user('approved'), user('rejected'), user('pending')]);
    expect(groups.pending).toHaveLength(2);
    expect(groups.approved).toHaveLength(1);
    expect(groups.rejected).toHaveLength(1);
  });

  it('lets members control PR CI only', () => {
    expect(canControl('admin', 'release')).toBe(true);
    expect(canControl('member', 'ci')).toBe(true);
    expect(canControl('member', 'release')).toBe(false);
    expect(canControl('pending', 'ci')).toBe(false);
    const failed = { kind: 'failed', error: 'x' } as unknown as Parameters<typeof permittedActions>[2];
    expect(permittedActions('member', 'release', failed)).toEqual({
      cancel: false,
      resume: false,
      retry: false,
    });
    expect(permittedActions('admin', 'release', failed).retry).toBe(true);
    expect(permittedActions('member', 'ci', failed).retry).toBe(true);
  });
});
