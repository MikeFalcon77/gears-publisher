import React from 'react';
import { createRoot } from 'react-dom/client';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import App from './App';
import { TooltipProvider } from '@/components/ui/tooltip';
import './styles.css';

const client = new QueryClient({
  defaultOptions: { queries: { retry: 1, refetchOnWindowFocus: true }, mutations: { retry: false } },
});

createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <QueryClientProvider client={client}>
      <TooltipProvider delayDuration={300}>
        <App />
      </TooltipProvider>
    </QueryClientProvider>
  </React.StrictMode>,
);
