import type { ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { Bot, CircleDot, Info } from "lucide-react";

import { api } from "../api/client";
import type { Route } from "../App";
import { ThemeToggle } from "./ThemeToggle";

interface LayoutProps {
  currentRoute: Route;
  onNavigate: (next: Route) => void;
  children: ReactNode;
}

export function Layout({ currentRoute, onNavigate, children }: LayoutProps) {
  const daemonQuery = useQuery({
    queryKey: ["daemon"],
    queryFn: api.daemon,
    refetchInterval: 5_000,
  });
  const healthQuery = useQuery({
    queryKey: ["healthz"],
    queryFn: api.health,
    refetchInterval: 5_000,
  });

  const healthy = healthQuery.data?.ok === true;
  const daemon = daemonQuery.data;

  return (
    <div className="flex min-h-screen flex-col bg-neutral-50 dark:bg-slate-900">
      <header className="border-b border-neutral-200 bg-white dark:border-slate-700 dark:bg-slate-800">
        <div className="mx-auto flex h-14 max-w-7xl items-center justify-between px-6">
          <div className="flex items-center gap-3">
            <Bot className="h-6 w-6 text-sky-500 dark:text-sky-400" aria-hidden="true" />
            <h1 className="text-lg font-semibold tracking-tight text-neutral-900 dark:text-slate-100">
              videocall{" "}
              <span className="font-normal text-neutral-500 dark:text-slate-400">bots-app</span>
            </h1>
            <ThemeToggle />
          </div>
          <div
            className="flex items-center gap-2 text-sm"
            data-testid="daemon-status"
            data-healthy={healthy}
          >
            <CircleDot
              className={`h-3 w-3 ${
                healthy
                  ? "text-emerald-500 dark:text-emerald-400"
                  : "text-red-500 dark:text-red-400"
              }`}
              aria-hidden="true"
            />
            <span className="text-neutral-600 dark:text-slate-300">
              {daemon ? (
                <>
                  ctl :{daemon.port}{" "}
                  <span className="font-mono text-xs text-neutral-400 dark:text-slate-500">
                    pid {daemon.pid}
                  </span>
                </>
              ) : (
                "discovering ctl daemon…"
              )}
            </span>
          </div>
        </div>
      </header>

      <div className="mx-auto flex w-full max-w-7xl flex-1 gap-6 px-6 py-6">
        <nav aria-label="Primary" className="w-44 shrink-0">
          <ul className="flex flex-col gap-1">
            <li>
              <NavItem
                active={currentRoute === "bots"}
                onClick={() => onNavigate("bots")}
                icon={<Bot className="h-4 w-4" />}
                label="Bots"
              />
            </li>
            <li>
              <NavItem
                active={currentRoute === "about"}
                onClick={() => onNavigate("about")}
                icon={<Info className="h-4 w-4" />}
                label="About"
              />
            </li>
          </ul>
        </nav>
        <main className="min-w-0 flex-1">{children}</main>
      </div>
    </div>
  );
}

interface NavItemProps {
  active: boolean;
  onClick: () => void;
  icon: ReactNode;
  label: string;
}

function NavItem({ active, onClick, icon, label }: NavItemProps) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={`flex w-full items-center gap-2 rounded-lg px-3 py-2 text-left text-sm transition-colors ${
        active
          ? "bg-sky-50 font-medium text-sky-700 dark:bg-sky-900/40 dark:text-sky-200"
          : "text-neutral-600 hover:bg-neutral-100 hover:text-neutral-900 dark:text-slate-300 dark:hover:bg-slate-700 dark:hover:text-slate-100"
      }`}
      aria-current={active ? "page" : undefined}
    >
      {icon}
      {label}
    </button>
  );
}
