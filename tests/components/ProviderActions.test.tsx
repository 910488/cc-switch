import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ProviderActions } from "@/components/providers/ProviderActions";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, options?: { count?: number }) => {
      const labels: Record<string, string> = {
        "codex.configureModels": "Configure Models",
        "codex.configureModelsHint": "Choose Codex models",
        "provider.enable": "Enable",
        "provider.inUse": "In Use",
        "failover.addQueue": "Add to failover",
      };

      if (key === "codex.modelsConfigured") {
        return `${options?.count ?? 0} models added`;
      }

      return labels[key] ?? key;
    },
  }),
}));

function renderActions(
  overrides: Partial<React.ComponentProps<typeof ProviderActions>> = {},
) {
  const props: React.ComponentProps<typeof ProviderActions> = {
    appId: "codex",
    isCurrent: false,
    isProxyTakeover: true,
    onSwitch: vi.fn(),
    onEdit: vi.fn(),
    onDuplicate: vi.fn(),
    onDelete: vi.fn(),
    onConfigureModels: vi.fn(),
    ...overrides,
  };

  render(<ProviderActions {...props} />);
  return props;
}

describe("ProviderActions Codex coexistence mode", () => {
  it("uses the main button to configure a third-party provider's Codex models", () => {
    const props = renderActions();

    fireEvent.click(screen.getByRole("button", { name: "Configure Models" }));

    expect(props.onConfigureModels).toHaveBeenCalledTimes(1);
    expect(props.onSwitch).not.toHaveBeenCalled();
  });

  it("shows the number of models already added to Codex", () => {
    renderActions({ codexConfiguredModelCount: 3 });

    expect(
      screen.getByRole("button", { name: "3 models added" }),
    ).toBeInTheDocument();
  });

  it("hides the obsolete enable button on the official provider card", () => {
    renderActions({ isOfficial: true, onConfigureModels: undefined });

    expect(screen.queryByText("Enable")).not.toBeInTheDocument();
  });

  it("keeps failover queue controls when automatic failover mode is enabled", () => {
    renderActions({
      isAutoFailoverEnabled: true,
      isInFailoverQueue: false,
      onToggleFailover: vi.fn(),
    });

    expect(
      screen.getByRole("button", { name: "Add to failover" }),
    ).toBeInTheDocument();
    expect(screen.queryByText("Configure Models")).not.toBeInTheDocument();
  });
});
