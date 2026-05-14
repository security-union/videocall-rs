import { useState } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import * as Toast from "@radix-ui/react-toast";

import { Layout } from "./components/Layout";
import { BotsPage } from "./pages/BotsPage";
import { AboutPage } from "./pages/AboutPage";
import { HelpPage } from "./pages/HelpPage";
import { ThemeProvider } from "./lib/theme";

/**
 * TanStack Query client with conservative defaults for a control-plane
 * UI: no automatic retries (the user gets a toast on failure), no
 * window-focus refetching (the bot table already polls every 2.5s), and
 * a short `gcTime` so memory doesn't grow over a long-lived session.
 */
const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: false,
      refetchOnWindowFocus: false,
      gcTime: 60_000,
    },
    mutations: {
      retry: false,
    },
  },
});

export type Route = "bots" | "help" | "about";

export function App() {
  const [route, setRoute] = useState<Route>("bots");
  let page;
  if (route === "bots") {
    page = <BotsPage />;
  } else if (route === "help") {
    page = <HelpPage />;
  } else {
    page = <AboutPage />;
  }
  return (
    <ThemeProvider>
      <QueryClientProvider client={queryClient}>
        <Toast.Provider swipeDirection="right" duration={5000}>
          <Layout currentRoute={route} onNavigate={setRoute}>
            {page}
          </Layout>
          <Toast.Viewport className="fixed bottom-4 right-4 z-50 flex w-96 flex-col gap-2 outline-none" />
        </Toast.Provider>
      </QueryClientProvider>
    </ThemeProvider>
  );
}
