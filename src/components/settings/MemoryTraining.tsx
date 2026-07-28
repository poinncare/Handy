import React, { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { useSettings } from "../../hooks/useSettings";
import { Slider } from "../ui/Slider";
import { ToggleSwitch } from "../ui/ToggleSwitch";

interface MemoryTrainingProps {
  descriptionMode?: "tooltip" | "inline";
  grouped?: boolean;
}

export const MemoryTraining: React.FC<MemoryTrainingProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();
  const enabled = getSetting("memory_training_enabled") ?? false;
  const thresholdSecs = getSetting("memory_training_threshold_secs") ?? 5;
  const [thresholdDraft, setThresholdDraft] = useState(thresholdSecs);
  const pendingThresholdRef = useRef<number | null>(null);
  const thresholdTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const thresholdUpdateChainRef = useRef<Promise<void>>(Promise.resolve());

  const enqueueThresholdUpdate = useCallback(
    (value: number) => {
      thresholdUpdateChainRef.current = thresholdUpdateChainRef.current
        .catch(() => undefined)
        .then(() => updateSetting("memory_training_threshold_secs", value));
    },
    [updateSetting],
  );

  const flushPendingThreshold = useCallback(() => {
    if (thresholdTimerRef.current !== null) {
      clearTimeout(thresholdTimerRef.current);
      thresholdTimerRef.current = null;
    }

    const pendingValue = pendingThresholdRef.current;
    pendingThresholdRef.current = null;
    if (pendingValue !== null) {
      enqueueThresholdUpdate(pendingValue);
    }
  }, [enqueueThresholdUpdate]);

  const handleThresholdChange = (value: number) => {
    setThresholdDraft(value);
    pendingThresholdRef.current = value;
    if (thresholdTimerRef.current !== null) {
      clearTimeout(thresholdTimerRef.current);
    }
    thresholdTimerRef.current = setTimeout(flushPendingThreshold, 200);
  };

  useEffect(() => {
    if (pendingThresholdRef.current === null) {
      setThresholdDraft(thresholdSecs);
    }
  }, [thresholdSecs]);

  useEffect(
    () => () => {
      flushPendingThreshold();
    },
    [flushPendingThreshold],
  );

  return (
    <>
      <ToggleSwitch
        checked={enabled}
        onChange={(value) => updateSetting("memory_training_enabled", value)}
        isUpdating={isUpdating("memory_training_enabled")}
        label={t("settings.advanced.memoryTraining.enabled.label")}
        description={t("settings.advanced.memoryTraining.enabled.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
      <Slider
        value={thresholdDraft}
        onChange={handleThresholdChange}
        min={1}
        max={60}
        step={1}
        disabled={!enabled}
        label={t("settings.advanced.memoryTraining.threshold.label")}
        description={t(
          "settings.advanced.memoryTraining.threshold.description",
        )}
        descriptionMode={descriptionMode}
        grouped={grouped}
        formatValue={(value) =>
          t("settings.advanced.memoryTraining.threshold.seconds", {
            count: value,
          })
        }
      />
    </>
  );
};
