import { useEffect, useState } from 'react';

export type Route = { page: 'runs' } | { page: 'run'; id: string };

export function parse(pathname: string): Route {
  const match = /^\/runs\/([0-9a-f-]{36})\/?$/i.exec(pathname);
  return match ? { page: 'run', id: match[1].toLowerCase() } : { page: 'runs' };
}

export function navigate(path: string): void {
  window.history.pushState(null, '', path);
  window.dispatchEvent(new PopStateEvent('popstate'));
}

export function useRoute(): Route {
  const [route, setRoute] = useState(() => parse(window.location.pathname));
  useEffect(() => {
    const update = () => setRoute(parse(window.location.pathname));
    window.addEventListener('popstate', update);
    return () => window.removeEventListener('popstate', update);
  }, []);
  return route;
}
