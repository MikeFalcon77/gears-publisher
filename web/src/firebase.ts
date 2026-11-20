import { initializeApp } from 'firebase/app';
import {
  getAuth,
  GoogleAuthProvider,
  onAuthStateChanged,
  signInWithPopup,
  signOut,
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
  emailVerified: boolean;
}

function identity(user: User | null): Identity | null {
  return user
    ? { uid: user.uid, email: user.email ?? '', emailVerified: user.emailVerified }
    : null;
}

export function watchUser(callback: (user: Identity | null) => void): () => void {
  if (!auth) {
    callback(null);
    return () => {};
  }
  return onAuthStateChanged(auth, (user) => callback(identity(user)));
}

export async function login(): Promise<void> {
  if (!auth) throw new Error('Firebase is not configured (VITE_FIREBASE_* at build time)');
  await signInWithPopup(auth, new GoogleAuthProvider());
}

export async function logout(): Promise<void> {
  if (auth) await signOut(auth);
}

export async function token(force = false): Promise<string | null> {
  return (await auth?.currentUser?.getIdToken(force)) ?? null;
}
