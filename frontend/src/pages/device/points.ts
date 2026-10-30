import type { DataType, DeviceModel, Latest } from "../../api/devices.ts";

/** A data point the page lists and can chart. */
export interface PointInfo {
  key: string;
  description: string | null;
  /** From the model; unknown for a point that only the stored readings name. */
  dataType: DataType | null;
  /** Whether the current model still has it: readings stay stored after a model drops a point. */
  inModel: boolean;
}

/**
 * The device's data points: the model's, in its order, then any point that has stored readings
 * but is no longer in the model. Without the model (it failed to load), the stored readings' points.
 */
export function chartablePoints(model: DeviceModel | undefined, latest: Latest | undefined): PointInfo[] {
  const points: PointInfo[] = (model?.points ?? []).map((point) => ({
    key: point.key,
    description: point.description?.trim() || null,
    dataType: point.data_type,
    inModel: true,
  }));
  const known = new Set(points.map((point) => point.key));
  for (const reading of latest?.points ?? []) {
    if (!known.has(reading.point)) {
      known.add(reading.point);
      points.push({ key: reading.point, description: null, dataType: null, inModel: model === undefined });
    }
  }
  return points;
}

/**
 * The point the chart shows: the address bar's when the device has it, otherwise the first that
 * is not text (a chart of text says little), otherwise the first. `null` when there is none.
 */
export function choosePoint(requested: string | null, points: readonly PointInfo[]): string | null {
  if (requested !== null && points.some((point) => point.key === requested)) return requested;
  return (points.find((point) => point.dataType !== "string") ?? points[0])?.key ?? null;
}
