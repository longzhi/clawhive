import { useAuthCheck } from "@/hooks/use-api";
import Login from "@/pages/Login";
import SetPassword from "@/pages/SetPassword";

export function AuthGuard({ children }: { children: React.ReactNode }) {
  const { data, isLoading, isError } = useAuthCheck();

  // Loading — show nothing (brief flash)
  if (isLoading) return null;

  // 401 means password is set but user isn't authenticated → show login
  if (isError) return <Login />;

  // No password configured yet → force password setup
  if (data && !data.auth_required) return <SetPassword />;

  // Password set but not authenticated → show login
  if (data && !data.authenticated) return <Login />;

  // Authenticated → show app
  return <>{children}</>;
}
