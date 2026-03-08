export async function apiFetch<T>(path: string, options?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    ...options,
    headers: { 'Content-Type': 'application/json', ...options?.headers },
  });
  if (!res.ok) {
    let errorMessage = `API error: ${res.status}`;
    try {
      const body = await res.text();
      if (body) {
        try {
          const json = JSON.parse(body);
          errorMessage = json.error || json.message || body || errorMessage;
        } catch {
          errorMessage = body || errorMessage;
        }
      }
    } catch {
      // If reading body fails, use default error message
    }
    throw new Error(errorMessage);
  }
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  if (!text) return undefined as T;
  try {
    return JSON.parse(text) as T;
  } catch {
    throw new Error(`Unexpected response from ${path}`);
  }
}
