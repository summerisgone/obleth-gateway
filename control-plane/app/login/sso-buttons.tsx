"use client";

import { Button } from "@/components/ui/button";
import { authClient } from "@/lib/auth/client";
import type { OidcProviderLabel } from "@/lib/auth/providers";

interface SsoButtonsProps {
  providers: OidcProviderLabel[];
  next?: string;
}

export function SsoButtons({ providers, next }: SsoButtonsProps) {
  if (providers.length === 0) return null;
  return (
    <div className="space-y-2">
      {providers.map((p) => (
        <Button
          key={p.providerId}
          type="button"
          variant="outline"
          className="w-full"
          onClick={() =>
            authClient.signIn.oauth2({ providerId: p.providerId, callbackURL: next ?? "/" })
          }
        >
          Sign in with {p.displayName}
        </Button>
      ))}
    </div>
  );
}
