# Self-hosting anisette in a macOS virtual machine

Signing in to Apple needs **anisette** data that identifies the computer. Since October 2026
Apple only accepts anisette that comes from a real Mac (or a macOS virtual machine); the data
third-party sideloaders used to generate is now refused for sign-in. The most reliable, fully
self-hosted way to get Mac anisette, without a spare physical Mac, is a small macOS virtual
machine that you run yourself: nothing leaves your network, and there is no third party involved.

> You only need this if you sign in with an Apple ID and don't already have a Mac on macOS 26 or
> earlier to serve anisette. With a **paid** developer certificate and profile you don't sign in
> at all, and third-party anisette servers remain supported if you'd rather use one — see
> [Anisette](../README.md#anisette).

This takes about 20–30 minutes, most of it the macOS install. You need:

- An **Apple-silicon Mac** (M1 or later) to host the VM.
- **[UTM](https://mac.getutm.app)** (free). The App Store build works too.
- About **40 GB** of free disk.

## 1. Create the macOS VM

1. Open UTM and click **Create a New Virtual Machine → Virtualize → macOS 12+**.
2. Let UTM **download the recommended macOS**, or point it at a macOS IPSW you already have.
   Any version from **macOS 26** onwards works.
3. Give it the defaults (4 GB RAM and ~40 GB disk are plenty) and finish. UTM installs macOS;
   this is the slow part.

## 2. Go through Setup Assistant

When the VM boots into Setup Assistant, set it up as normal:

- Create a local account (any name and password you like — you'll need this password once, in
  step 4). When creating the user, ensure the checkbox to allow password recovery via Apple ID is unchecked.
- **Skip** signing in to an Apple Account, Siri, Screen Time, and analytics. None are needed.

## 3. Turn on automatic login

The anisette server has to run inside a logged-in desktop session, so the VM must log itself in
after a restart.

- Open **System Settings → Users & Groups**.
- Set **Automatically log in as** to the account you just made, and enter its password.

(If that option is greyed out, it's usually because FileVault was enabled during setup - disable it.)

## 4. Run the setup script in recoveryOS

The script needs recoveryOS to turn off two security settings (this is safe on an internal VM and
is what lets macOS run Apparition's signed anisette helper).

1. Shut the VM down (**⌘Q** or Apple menu → Shut Down).
2. Boot it into recovery: in UTM, click the **⌄** next to the Run button (or right-click the VM)
   and choose **Boot into Recovery**. If you don't see that, run this in the host Mac's Terminal:
   ```sh
   utmctl start --recovery "<your VM's name>"
   ```
3. In recoveryOS, open **Utilities → Terminal** and run:
   ```sh
   bash -c "$(curl -fsSL https://apparition.illixion.com/avm.sh)"
   ```
4. It will:
   - turn **SIP** off — macOS asks for a password: enter the account password from step 2;
   - turn **AMFI** off (a boot-arg);
   - download and install the anisette server;
   - offer to **restart**. Let it.

## 5. Make the VM reachable

By default UTM gives the VM a private address (**Shared Network**) that only the host Mac can
reach — fine if the desktop app runs on the host and relays anisette as the companion, but not for
pointing a device straight at the VM. To reach it directly, pick one in the VM's UTM settings
(**Network → Network Mode**):

- **Bridged** — the VM gets an address on your LAN, reachable by any device on the same Wi-Fi.
  Simplest at home.
- **Tailscale** — install [Tailscale](https://tailscale.com) inside the VM and sign in to your
  tailnet. The VM is then reachable securely from anywhere, encrypted and with no ports exposed;
  use its Tailscale address. This is also how the companion reaches it when you're away from home.

## 6. Point Apparition at it

After the restart the VM logs itself in and starts serving anisette on **port 6969**.

1. Find the VM's address — in the VM's Terminal (`ipconfig getifaddr en0` for a bridged LAN
   address, or `tailscale ip -4` for its Tailscale address).
2. In Apparition, set the anisette server to `http://<that address>:6969`:
   - **On a device:** Setup → Anisette → enter the address.
   - **Through the companion:** the desktop app on the host reaches the VM and offers its anisette
     to your devices over Tailscale, so it works away from home too.

Keep the VM running while you sign in or refresh. In UTM you can set the VM to start automatically
so it's always available.

## Troubleshooting

- **`csrutil disable` asks for a password and rejects it.** It wants the account password from
  Setup Assistant. If it says it can't authenticate at all, Setup Assistant probably didn't finish
  creating an account — complete step 2 first.
- **Sign-in fails with `-3000076`.** The server is running but not inside a desktop session. Check
  that automatic login is on (step 3) and that the VM actually logged in after its restart.
- **Sign-in fails with a `503`.** That's a transient hiccup on Apple's side; Apparition retries,
  and trying again usually works.
- **Check the server.** In the VM's Terminal:
  ```sh
  curl -s http://localhost:6969/v3/client_info   # should name your VM's Mac model
  tail /tmp/apparition-anisette.log
  ```

The script is [`selfhost/macos-vm/setup.sh`](../selfhost/macos-vm/setup.sh); it only uses what
recoveryOS ships, creates no account, and installs a single launch item. A real Mac on macOS 26 or
earlier needs none of this — it serves its own anisette, and you can run the same script there from
Terminal with `sudo` (it leaves SIP alone).
