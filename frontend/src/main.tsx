import "./styles/global.css";

import { ROUTES } from "./app/routes.tsx";
import { startApp } from "./app/start.tsx";

startApp({ routes: ROUTES });
