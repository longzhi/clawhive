import { useAuthCheck } from "@/hooks/use-api";
import Login from "@/pages/Login";

export function AuthGuard({ children }: { children: React.ReactNode }) {
  const { data, isLoading, isError } = useAuthCheck();

  // Loading — show nothing (brief flash)
  if (isLoading) return null;

  // 401 means password is set but user isn't authenticated → show login
  if (isError) return <Login />;

  // authenticated or no password configured → show app
  return <>{children}</>;
}
