import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Link } from "react-router";

import { useAssetsApi } from "../../api/AssetsApiProvider.tsx";
import { ApiError } from "../../api/errors.ts";
import { devicesOfModelPath } from "../../app/paths.ts";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "../../components/ui/alert-dialog.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { tenantModelsKey } from "../device/queries.ts";

/**
 * Confirms and deletes a model (`devices--model-delete-in-use`): the focus starts on Cancel. A
 * model a device uses is refused with 409 `device_model.in_use`; the dialog then says so, links to
 * the devices that use it, and offers only to close. Any other refusal shows in the dialog.
 */
export function DeleteModelDialog(props: {
  model: { model_id: string; name: string } | null;
  onClose(): void;
  onDeleted(name: string): void;
}) {
  const { t, errorMessage } = useI18n();
  const api = useAssetsApi();
  const queryClient = useQueryClient();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const { model } = props;
  const mutation = useMutation({
    mutationFn: (target: { model_id: string; name: string }) => api.deleteDeviceModel(target.model_id),
    onSuccess: async (_none, target) => {
      await queryClient.invalidateQueries({ queryKey: tenantModelsKey(tenant) });
      props.onDeleted(target.name);
    },
  });
  const inUse = mutation.error instanceof ApiError && mutation.error.body?.message_key === "device_model.in_use";
  return (
    <AlertDialog
      open={model !== null}
      onOpenChange={(open) => {
        if (open || mutation.isPending) return;
        mutation.reset();
        props.onClose();
      }}
    >
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{t("model_delete.title", { name: model?.name ?? "" })}</AlertDialogTitle>
          <AlertDialogDescription>{t("model_delete.body")}</AlertDialogDescription>
        </AlertDialogHeader>
        {inUse && model ? (
          <Alert variant="destructive" className="grid-cols-[1fr]">
            <AlertTitle className="col-start-1">{t("model_delete.in_use_title")}</AlertTitle>
            <AlertDescription className="col-start-1">
              <p>{t("model_delete.in_use_body")}</p>
              <Link
                to={devicesOfModelPath(model.model_id)}
                className={cn("rounded-sm underline underline-offset-4", focusRing)}
              >
                {t("model_delete.in_use_link", { name: model.name })}
              </Link>
            </AlertDescription>
          </Alert>
        ) : mutation.isError ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{errorMessage(mutation.error)}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={mutation.isPending}>
            {inUse ? t("model_delete.close") : t("device_form.cancel")}
          </AlertDialogCancel>
          <Button
            variant="destructive"
            disabled={mutation.isPending || model === null || inUse}
            onClick={() => model && mutation.mutate(model)}
          >
            {mutation.isPending ? <Spinner /> : null}
            {mutation.isPending ? t("model_delete.deleting") : t("model_delete.confirm")}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
