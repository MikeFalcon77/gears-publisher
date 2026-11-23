import { initializeApp } from 'firebase/app';
import {
  getAuth,
  GithubAuthProvider,
  GoogleAuthProvider,
  onAuthStateChanged,
  signInWithPopup,
  signOut,
  type AuthProvider,
  type User,
} from 'firebase/auth';

const config = {
  apiKey: import.meta.env.VITE_FIREBASE_API_KEY,
  authDomain: import.meta.env.VITE_FIREBASE_AUTH_DOMAIN,
  projectId: import.meta.env.VITE_FIREBASE_PROJECT_ID,
};

export const configured = Boolean(config.apiKey && config.authDomain && config.projectId);
const auth = configured ? getAuth(initializeApp(config)) : null;

export interface Identity {
  uid: string;
  email: string;
  displayName: string;
}

export type Provider = 'google' | 'github';

function identity(user: User | null): Identity | null {
  return user
    ? {
        uid: user.uid,
        email: user.email ?? '',
        displayName: user.displayName ?? '',
      }
    : null;
}

export function watchUser(callback: (user: Identity | null) => void): () => void {
  if (!auth) {
    callback(null);
    return () => {};
  }
  return onAuthStateChanged(auth, (user) => callback(identity(user)));
}

function provider(name: Provider): AuthProvider {
  if (name === 'google') return new GoogleAuthProvider();
  const github = new GithubAuthProvider();
  github.addScope('user:email');
  return github;
}

export async function login(name: Provider): Promise<void> {
  if (!auth) throw new Error('Firebase is not configured (VITE_FIREBASE_* at build time)');
  try {
    await signInWithPopup(auth, provider(name));
  } catch (error) {
    throw new Error(signInError(error));
  }
}

/** What to tell a person whose sign-in failed. */
export function signInError(error: unknown): string {
  const code = (error as { code?: string } | null)?.code;
  switch (code) {
    case 'auth/account-exists-with-different-credential':
      return 'This email already signs in with another provider. Use that one (Google or GitHub).';
    case 'auth/popup-closed-by-user':
    case 'auth/cancelled-popup-request':
      return 'Sign-in was cancelled.';
    case 'auth/popup-blocked':
      return 'The browser blocked the sign-in window; allow pop-ups for this site.';
    case 'auth/operation-not-allowed':
      return 'This sign-in provider is not enabled for the publisher yet.';
    default:
      return error instanceof Error ? error.message : String(error);
  }
}

export async function logout(): Promise<void> {
  if (auth) await signOut(auth);
}

export async function token(force = false): Promise<string | null> {
  return (await auth?.currentUser?.getIdToken(force)) ?? null;
}
