"""A-370, the chosen-path launcher's AppArmor option from Python (§4.6 "The launcher"): `slates.exec_command` builds the
command that runs `slates exec`, and `slates.apparmor_profile` renders slates' AppArmor profile for an executable. Both
are the Rust surfaces' own (`slates_client::exec`), so these tests hold the Python plumbing: the option off unless asked,
on exactly when asked, and the profile's text and refusals. Run with the built extension:
`python3 -m unittest discover crates/sdk-python/tests`."""

import unittest

import slates


class ExecCommandTest(unittest.TestCase):
    def test_the_apparmor_option_is_off_unless_asked_and_on_when_asked(self):
        """Do: build the launcher's command with the option left out and with it `True`. Expect: the `slates`
        binary, the launcher's grammar with each value after `=`, and `--apparmor-profile` only when asked."""
        self.assertEqual(
            slates.exec_command("scratch", "/home/u/build", ["cargo", "build", "--", "-x"]),
            ["slates", "exec", "--volume=scratch", "--at=/home/u/build", "--", "cargo", "build", "--", "-x"],
        )
        self.assertEqual(
            slates.exec_command(
                "scratch",
                "/home/u/build",
                ["cargo", "build"],
                apparmor_profile=True,
                program="/usr/local/bin/slates",
            ),
            [
                "/usr/local/bin/slates",
                "exec",
                "--volume=scratch",
                "--at=/home/u/build",
                "--apparmor-profile",
                "--",
                "cargo",
                "build",
            ],
        )
        self.assertNotIn(
            "--apparmor-profile",
            slates.exec_command("v", "/p", ["true"], apparmor_profile=False),
        )

    def test_the_option_is_keyword_only_and_an_empty_command_is_refused(self):
        """Do: pass the option positionally, and an empty command. Expect: a `TypeError` (the option is named, so
        it is never turned on by position) and a `SlatesError`."""
        with self.assertRaises(TypeError):
            slates.exec_command("v", "/p", ["true"], True)
        with self.assertRaises(slates.SlatesError):
            slates.exec_command("v", "/p", [])


class AppArmorProfileTest(unittest.TestCase):
    def test_the_profile_names_the_executable_and_grants_user_namespaces(self):
        """Do: render the profile for the installed binary's path, and for a path it cannot name exactly. Expect:
        the profile named by the path in unconfined mode granting `userns,`; a `SlatesError` naming why."""
        text = slates.apparmor_profile("/usr/local/bin/slates")
        self.assertIn("abi <abi/4.0>,", text)
        self.assertIn("profile /usr/local/bin/slates flags=(unconfined) {\n  userns,\n}", text)
        with self.assertRaises(slates.SlatesError) as refused:
            slates.apparmor_profile("/opt/my tools/slates")
        self.assertIn("cannot name exactly", str(refused.exception))


if __name__ == "__main__":
    unittest.main()
