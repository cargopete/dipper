import type { Metadata, Viewport } from "next";

import "./globals.css";

export const metadata: Metadata = {
  title: "Balerion",
  description: "Search for something to watch. Playback happens on your own machine.",
  /* Nothing here should end up in a search index. It is a private tool behind a
   * password, and a password-gated page in someone's results helps nobody. */
  robots: { index: false, follow: false },
  icons: {
    icon:
      "data:image/svg+xml," +
      "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'>" +
      "<rect width='16' height='16' rx='3' fill='%230a0a0b'/>" +
      "<circle cx='8' cy='8' r='3.5' fill='%23e0703c'/></svg>",
  },
};

export const viewport: Viewport = {
  colorScheme: "dark",
  themeColor: "#0a0a0b",
};

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
