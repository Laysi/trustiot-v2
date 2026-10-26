// The d3 locale definitions the sandbox formats with (frontend/sandbox/README.md, 「格式化」).
// The time definitions are d3-time-format 4.1.0's locale/zh-TW.json and locale/en-US.json, which
// the package does not export; they are copied here so the runtime and the comparison bundle the
// same data. Their texts hold no digits of their own except the zh-TW short month names, whose
// digits are the month's number.
import type { FormatLocaleDefinition } from "d3-format";
import type { TimeLocaleDefinition } from "d3-time-format";

export const NUMBER_LOCALE_DEFINITIONS: Record<"zh-TW" | "en", FormatLocaleDefinition> = {
  "zh-TW": { decimal: ".", thousands: ",", grouping: [3], currency: ["NT$", ""] },
  en: { decimal: ".", thousands: ",", grouping: [3], currency: ["$", ""] },
};

export const TIME_LOCALE_DEFINITIONS: Record<"zh-TW" | "en", TimeLocaleDefinition> = {
  "zh-TW": {
    dateTime: "%x %A %X",
    date: "%Y年%-m月%-d日",
    time: "%H:%M:%S",
    periods: ["上午", "下午"],
    days: ["星期日", "星期一", "星期二", "星期三", "星期四", "星期五", "星期六"],
    shortDays: ["日", "一", "二", "三", "四", "五", "六"],
    months: ["一月", "二月", "三月", "四月", "五月", "六月", "七月", "八月", "九月", "十月", "十一月", "十二月"],
    shortMonths: ["1月", "2月", "3月", "4月", "5月", "6月", "7月", "8月", "9月", "10月", "11月", "12月"],
  },
  en: {
    dateTime: "%x, %X",
    date: "%-m/%-d/%Y",
    time: "%-I:%M:%S %p",
    periods: ["AM", "PM"],
    days: ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"],
    shortDays: ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"],
    months: [
      "January",
      "February",
      "March",
      "April",
      "May",
      "June",
      "July",
      "August",
      "September",
      "October",
      "November",
      "December",
    ],
    shortMonths: ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"],
  },
};
