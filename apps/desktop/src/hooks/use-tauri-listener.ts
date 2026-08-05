import { useEffect, useRef } from "react";

type Cleanup = () => void;

export function useTauriListener<T>(
  register: (handler: (payload: T) => void) => Promise<Cleanup>,
  handler: (payload: T) => void,
  onError?: (error: unknown) => void,
  enabled = true,
) {
  const handlerRef = useRef(handler);
  const errorRef = useRef(onError);
  handlerRef.current = handler;
  errorRef.current = onError;

  useEffect(() => {
    if (!enabled) {
      return;
    }

    let disposed = false;
    let cleanup: Cleanup | undefined;

    void register((payload) => handlerRef.current(payload))
      .then((unlisten) => {
        if (disposed) {
          unlisten();
        } else {
          cleanup = unlisten;
        }
      })
      .catch((error: unknown) => {
        if (!disposed) {
          errorRef.current?.(error);
        }
      });

    return () => {
      disposed = true;
      cleanup?.();
    };
  }, [enabled, register]);
}
