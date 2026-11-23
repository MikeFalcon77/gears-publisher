import type { UserEntry } from './types';

/** Longest note on an access request; the server's `MAX_NOTE`. */
export const MAX_NOTE = 300;

/** `github.com` → `GitHub`. */
export function providerLabel(provider: string | null): string {
  switch (provider) {
    case 'google.com':
      return 'Google';
    case 'github.com':
      return 'GitHub';
    case null:
      return '—';
    default:
      return provider;
  }
}

/** How a request is named in the admin lists. */
export function who(user: Pick<UserEntry, 'name' | 'email' | 'uid'>): string {
  return user.name ?? user.email ?? user.uid;
}

/** Requests by status, each newest first (the server's order). */
export function byStatus(users: UserEntry[]): Record<UserEntry['status'], UserEntry[]> {
  return {
    pending: users.filter((u) => u.status === 'pending'),
    approved: users.filter((u) => u.status === 'approved'),
    rejected: users.filter((u) => u.status === 'rejected'),
  };
}
