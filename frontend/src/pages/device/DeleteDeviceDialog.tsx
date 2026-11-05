import { useMutation, useQueryClient } from "@tanstack/react-query";

import { useAssetsApi } from "../../api/AssetsApiProvider.tsx";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "../../components/ui/alert-dialog.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { tenantDevicesKey } from "./queries.ts";

/**
 * Confirms and deletes a device (docs/design/tenant-admin/, `devices--device-delete`): the focus
 * starts on Cancel, the button names the action, a refusal shows in the dialog, which stays open,
 * and while the request runs the dialog cannot be closed.
 */
export function DeleteDeviceDialog(props: {
  device: { device_id: string; name: string } | null;
  onClose(): void;
  onDeleted(name: string): void;
}) {
  const { t, errorMessage } = useI18n();
  const api = useAssetsApi();
  const queryClient = useQueryClient();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const { device } = props;
  const mutation = useMutation({
    mutationFn: (target: { device_id: string; name: string }) => api.deleteDevice(target.device_id),
    onSuccess: async (_none, target) => {
      await queryClient.invalidateQueries({ queryKey: tenantDevicesKey(tenant) });
      props.onDeleted(target.name);
    },
  });
  return (
    <AlertDialog
      open={device !== null}
      onOpenChange={(open) => {
        if (open || mutation.isPending) return;
        mutation.reset();
        props.onClose();
      }}
    >
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{t("device_delete.title", { name: device?.name ?? "" })}</AlertDialogTitle>
          <AlertDialogDescription>{t("device_delete.body")}</AlertDialogDescription>
        </AlertDialogHeader>
        {mutation.isError ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{errorMessage(mutation.error)}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={mutation.isPending}>{t("device_form.cancel")}</AlertDialogCancel>
          <Button
            variant="destructive"
            disabled={mutation.isPending || device === null}
            onClick={() => device && mutation.mutate(device)}
          >
            {mutation.isPending ? <Spinner /> : null}
            {mutation.isPending ? t("device_delete.deleting") : t("device_delete.confirm")}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
